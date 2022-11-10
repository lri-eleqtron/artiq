use core::str;
use core::slice;
use cslice::{CSlice, CMutSlice};
use byteorder::{NativeEndian, ByteOrder};
use io::{ProtoRead, Read, Write, ProtoWrite, Error};
use self::tag::{Tag, TagIterator, split_tag};

#[inline]
fn round_up(val: usize, power_of_two: usize) -> usize {
    assert!(power_of_two.is_power_of_two());
    let max_rem = power_of_two - 1;
    (val + max_rem) & (!max_rem)
}

#[inline]
unsafe fn round_up_mut<T>(ptr: *mut T, power_of_two: usize) -> *mut T {
    round_up(ptr as usize, power_of_two) as *mut T
}

#[inline]
unsafe fn round_up_const<T>(ptr: *const T, power_of_two: usize) -> *const T {
    round_up(ptr as usize, power_of_two) as *const T
}

#[inline]
unsafe fn align_ptr<T>(ptr: *const ()) -> *const T {
    round_up_const(ptr, core::mem::align_of::<T>()) as *const T
}

#[inline]
unsafe fn align_ptr_mut<T>(ptr: *mut ()) -> *mut T {
    round_up_mut(ptr, core::mem::align_of::<T>()) as *mut T
}

/// Reads (deserializes) `length` array or list elements of type `tag` from `reader`,
/// writing them into the buffer given by `storage`.
///
/// `alloc` is used for nested allocations (if elements themselves contain
/// lists/arrays), see [recv_value].
unsafe fn recv_elements<R, E>(
    reader: &mut R,
    tag: Tag,
    length: usize,
    storage: *mut (),
    alloc: &dyn Fn(usize) -> Result<*mut (), E>,
) -> Result<(), E>
where
    R: Read + ?Sized,
    E: From<Error<R::ReadError>>,
{
    // List of simple types are special-cased in the protocol for performance.
    match tag {
        Tag::Bool => {
            let dest = slice::from_raw_parts_mut(storage as *mut u8, length);
            reader.read_exact(dest)?;
        },
        Tag::Int32 => {
            let dest = slice::from_raw_parts_mut(storage as *mut u8, length * 4);
            reader.read_exact(dest)?;
            let dest = slice::from_raw_parts_mut(storage as *mut i32, length);
            NativeEndian::from_slice_i32(dest);
        },
        Tag::Int64 | Tag::Float64 => {
            let dest = slice::from_raw_parts_mut(storage as *mut u8, length * 8);
            reader.read_exact(dest)?;
            let dest = slice::from_raw_parts_mut(storage as *mut i64, length);
            NativeEndian::from_slice_i64(dest);
        },
        _ => {
            let mut data = storage;
            for _ in 0..length {
                recv_value(reader, tag, &mut data, alloc)?
            }
        }
    }
    Ok(())
}

/// Reads (deserializes) a value of type `tag` from `reader`, writing the results to
/// the kernel-side buffer `data` (the passed pointer to which is incremented to point
/// past the just-received data). For nested allocations (lists/arrays), `alloc` is
/// invoked any number of times with the size of the required allocation as a parameter
/// (which is assumed to be correctly aligned for all payload types).
unsafe fn recv_value<R, E>(reader: &mut R, tag: Tag, data: &mut *mut (),
                           alloc: &dyn Fn(usize) -> Result<*mut (), E>)
                          -> Result<(), E>
    where R: Read + ?Sized,
          E: From<Error<R::ReadError>>
{
    macro_rules! consume_value {
        ($ty:ty, |$ptr:ident| $map:expr) => ({
            let $ptr = align_ptr_mut::<$ty>(*data) as *mut $ty;
            *data = $ptr.offset(1) as *mut ();
            $map
        })
    }

    match tag {
        Tag::None => Ok(()),
        Tag::Bool =>
            consume_value!(u8, |ptr| {
                *ptr = reader.read_u8()?; Ok(())
            }),
        Tag::Int32 =>
            consume_value!(u32, |ptr| {
                *ptr = reader.read_u32()?; Ok(())
            }),
        Tag::Int64 | Tag::Float64 =>
            consume_value!(u64, |ptr| {
                *ptr = reader.read_u64()?; Ok(())
            }),
        Tag::String | Tag::Bytes | Tag::ByteArray => {
            consume_value!(CMutSlice<u8>, |ptr| {
                let length = reader.read_u32()? as usize;
                *ptr = CMutSlice::new(alloc(length)? as *mut u8, length);
                reader.read_exact((*ptr).as_mut())?;
                Ok(())
            })
        }
        Tag::Tuple(it, arity) => {
            let alignment = tag.alignment();
            *data = round_up_mut(*data, alignment);
            let mut it = it.clone();
            for _ in 0..arity {
                let tag = it.next().expect("truncated tag");
                recv_value(reader, tag, data, alloc)?
            }
            // Take into account any tail padding (if element(s) with largest alignment
            // are not at the end).
            *data = round_up_mut(*data, alignment);
            Ok(())
        }
        Tag::List(it) => {
            #[repr(C)]
            struct List { elements: *mut (), length: usize }
            consume_value!(*mut List, |ptr_to_list| {
                let tag = it.clone().next().expect("truncated tag");
                let length = reader.read_u32()? as usize;

                // To avoid multiple kernel CPU roundtrips, use a single allocation for
                // both the pointer/length List (slice) and the backing storage for the
                // elements. We can assume that alloc() is aligned suitably, so just
                // need to take into account any extra padding required.
                // (Note: On RISC-V, there will never actually be any types with
                // alignment larger than 8 bytes, so storage_offset == 0 always.)
                let list_size = 4 + 4;
                let storage_offset = round_up(list_size, tag.alignment());
                let storage_size = tag.size() * length;

                let allocation = alloc(storage_offset as usize + storage_size)? as *mut u8;
                *ptr_to_list = allocation as *mut List;
                let storage = allocation.offset(storage_offset as isize) as *mut ();

                (**ptr_to_list).length = length;
                (**ptr_to_list).elements = storage;
                recv_elements(reader, tag, length, storage, alloc)
            })
        }
        Tag::Array(it, num_dims) => {
            consume_value!(*mut (), |buffer| {
                // Deserialize length along each dimension and compute total number of
                // elements.
                let mut total_len: usize = 1;
                for _ in 0..num_dims {
                    let len = reader.read_u32()? as usize;
                    total_len *= len;
                    consume_value!(usize, |ptr| *ptr = len )
                }

                // Allocate backing storage for elements; deserialize them.
                let elt_tag = it.clone().next().expect("truncated tag");
                *buffer = alloc(elt_tag.size() * total_len)?;
                recv_elements(reader, tag, total_len, *buffer, alloc)
            })
        }
        Tag::Range(it) => {
            *data = round_up_mut(*data, tag.alignment());
            let tag = it.clone().next().expect("truncated tag");
            recv_value(reader, tag, data, alloc)?;
            recv_value(reader, tag, data, alloc)?;
            recv_value(reader, tag, data, alloc)?;
            Ok(())
        }
        Tag::Keyword(_) => unreachable!(),
        Tag::Object => unreachable!()
    }
}

pub fn recv_return<R, E>(reader: &mut R, tag_bytes: &[u8], data: *mut (),
                         alloc: &dyn Fn(usize) -> Result<*mut (), E>)
                        -> Result<(), E>
    where R: Read + ?Sized,
          E: From<Error<R::ReadError>>
{
    let mut it = TagIterator::new(tag_bytes);
    #[cfg(feature = "log")]
    debug!("recv ...->{}", it);

    let tag = it.next().expect("truncated tag");
    let mut data = data;
    unsafe { recv_value(reader, tag, &mut data, alloc)? };

    Ok(())
}

unsafe fn send_elements<W>(writer: &mut W, elt_tag: Tag, length: usize, data: *const ())
                          -> Result<(), Error<W::WriteError>>
    where W: Write + ?Sized
{
    writer.write_u8(elt_tag.as_u8())?;
    match elt_tag {
        // we cannot use NativeEndian::from_slice_i32 as the data is not mutable,
        // and that is not needed as the data is already in native endian
        Tag::Bool => {
            let slice = slice::from_raw_parts(data as *const u8, length);
            writer.write_all(slice)?;
        },
        Tag::Int32 => {
            let slice = slice::from_raw_parts(data as *const u8, length * 4);
            writer.write_all(slice)?;
        },
        Tag::Int64 | Tag::Float64 => {
            let slice = slice::from_raw_parts(data as *const u8, length * 8);
            writer.write_all(slice)?;
        },
        _ => {
            let mut data = data;
            for _ in 0..length {
                send_value(writer, elt_tag, &mut data)?;
            }
        }
    }
    Ok(())
}

unsafe fn send_value<W>(writer: &mut W, tag: Tag, data: &mut *const ())
                       -> Result<(), Error<W::WriteError>>
    where W: Write + ?Sized
{
    macro_rules! consume_value {
        ($ty:ty, |$ptr:ident| $map:expr) => ({
            let $ptr = align_ptr::<$ty>(*data);
            *data = $ptr.offset(1) as *const ();
            $map
        })
    }

    writer.write_u8(tag.as_u8())?;
    match tag {
        Tag::None => Ok(()),
        Tag::Bool =>
            consume_value!(u8, |ptr|
                writer.write_u8(*ptr)),
        Tag::Int32 =>
            consume_value!(u32, |ptr|
                writer.write_u32(*ptr)),
        Tag::Int64 | Tag::Float64 =>
            consume_value!(u64, |ptr|
                writer.write_u64(*ptr)),
        Tag::String =>
            consume_value!(CSlice<u8>, |ptr|
                writer.write_string(str::from_utf8((*ptr).as_ref()).unwrap())),
        Tag::Bytes | Tag::ByteArray =>
            consume_value!(CSlice<u8>, |ptr|
                writer.write_bytes((*ptr).as_ref())),
        Tag::Tuple(it, arity) => {
            let mut it = it.clone();
            writer.write_u8(arity)?;
            let mut max_alignment = 0;
            for _ in 0..arity {
                let tag = it.next().expect("truncated tag");
                max_alignment = core::cmp::max(max_alignment, tag.alignment());
                send_value(writer, tag, data)?
            }
            *data = round_up_const(*data, max_alignment);
            Ok(())
        }
        Tag::List(it) => {
            #[repr(C)]
            struct List { elements: *const (), length: u32 }
            consume_value!(&List, |ptr| {
                let length = (**ptr).length as usize;
                writer.write_u32((**ptr).length)?;
                let tag = it.clone().next().expect("truncated tag");
                send_elements(writer, tag, length, (**ptr).elements)
            })
        }
        Tag::Array(it, num_dims) => {
            writer.write_u8(num_dims)?;
            consume_value!(*const(), |buffer| {
                let elt_tag = it.clone().next().expect("truncated tag");

                let mut total_len = 1;
                for _ in 0..num_dims {
                    consume_value!(u32, |len| {
                        writer.write_u32(*len)?;
                        total_len *= *len;
                    })
                }
                let length = total_len as usize;
                send_elements(writer, elt_tag, length, *buffer)
            })
        }
        Tag::Range(it) => {
            let tag = it.clone().next().expect("truncated tag");
            send_value(writer, tag, data)?;
            send_value(writer, tag, data)?;
            send_value(writer, tag, data)?;
            Ok(())
        }
        Tag::Keyword(it) => {
            #[repr(C)]
            struct Keyword<'a> { name: CSlice<'a, u8> }
            consume_value!(Keyword, |ptr| {
                writer.write_string(str::from_utf8((*ptr).name.as_ref()).unwrap())?;
                let tag = it.clone().next().expect("truncated tag");
                let mut data = ptr.offset(1) as *const ();
                send_value(writer, tag, &mut data)
            })
            // Tag::Keyword never appears in composite types, so we don't have
            // to accurately advance data.
        }
        Tag::Object => {
            #[repr(C)]
            struct Object { id: u32 }
            consume_value!(*const Object, |ptr|
                writer.write_u32((**ptr).id))
        }
    }
}

pub fn send_args<W>(writer: &mut W, service: u32, tag_bytes: &[u8], data: *const *const ())
                   -> Result<(), Error<W::WriteError>>
    where W: Write + ?Sized
{
    let (arg_tags_bytes, return_tag_bytes) = split_tag(tag_bytes);

    let mut args_it = TagIterator::new(arg_tags_bytes);
    #[cfg(feature = "log")]
    {
        let return_it = TagIterator::new(return_tag_bytes);
        debug!("send<{}>({})->{}", service, args_it, return_it);
    }

    writer.write_u32(service)?;
    for index in 0.. {
        if let Some(arg_tag) = args_it.next() {
            let mut data = unsafe { *data.offset(index) };
            unsafe { send_value(writer, arg_tag, &mut data)? };
        } else {
            break
        }
    }
    writer.write_u8(0)?;
    writer.write_bytes(return_tag_bytes)?;

    Ok(())
}

mod tag {
    use core::fmt;
    use super::round_up;

    pub fn split_tag(tag_bytes: &[u8]) -> (&[u8], &[u8]) {
        let tag_separator =
            tag_bytes.iter()
                     .position(|&b| b == b':')
                     .expect("tag without a return separator");
        let (arg_tags_bytes, rest) = tag_bytes.split_at(tag_separator);
        let return_tag_bytes = &rest[1..];
        (arg_tags_bytes, return_tag_bytes)
    }

    #[derive(Debug, Clone, Copy)]
    pub enum Tag<'a> {
        None,
        Bool,
        Int32,
        Int64,
        Float64,
        String,
        Bytes,
        ByteArray,
        Tuple(TagIterator<'a>, u8),
        List(TagIterator<'a>),
        Array(TagIterator<'a>, u8),
        Range(TagIterator<'a>),
        Keyword(TagIterator<'a>),
        Object
    }

    impl<'a> Tag<'a> {
        pub fn as_u8(self) -> u8 {
            match self {
                Tag::None => b'n',
                Tag::Bool => b'b',
                Tag::Int32 => b'i',
                Tag::Int64 => b'I',
                Tag::Float64 => b'f',
                Tag::String => b's',
                Tag::Bytes => b'B',
                Tag::ByteArray => b'A',
                Tag::Tuple(_, _) => b't',
                Tag::List(_) => b'l',
                Tag::Array(_, _) => b'a',
                Tag::Range(_) => b'r',
                Tag::Keyword(_) => b'k',
                Tag::Object => b'O',
            }
        }

        pub fn alignment(self) -> usize {
            use cslice::CSlice;
            match self {
                Tag::None => 1,
                Tag::Bool => core::mem::align_of::<u8>(),
                Tag::Int32 => core::mem::align_of::<i32>(),
                Tag::Int64 => core::mem::align_of::<i64>(),
                Tag::Float64 => core::mem::align_of::<f64>(),
                // struct type: align to largest element
                Tag::Tuple(it, arity) => {
                    let it = it.clone();
                    it.take(arity.into()).map(|t| t.alignment()).max().unwrap()
                },
                Tag::Range(it) => {
                    let it = it.clone();
                    it.take(3).map(|t| t.alignment()).max().unwrap()
                }
                // the ptr/length(s) pair is basically CSlice
                Tag::Bytes | Tag::String | Tag::ByteArray | Tag::List(_) | Tag::Array(_, _) =>
                    core::mem::align_of::<CSlice<()>>(),
                // will not be sent from the host
                _ => unreachable!("unexpected tag from host")
            }
        }

        /// Returns the "alignment size" of a value with the type described by the tag
        /// (in bytes), i.e. the stride between successive elements in a list/array of
        /// the given type, or the offset from a struct element of this type to the
        /// next field.
        pub fn size(self) -> usize {
            match self {
                Tag::None => 0,
                Tag::Bool => 1,
                Tag::Int32 => 4,
                Tag::Int64 => 8,
                Tag::Float64 => 8,
                Tag::String => 8,
                Tag::Bytes => 8,
                Tag::ByteArray => 8,
                Tag::Tuple(it, arity) => {
                    let mut size = 0;
                    let mut max_alignment = 0;
                    let mut it = it.clone();
                    for _ in 0..arity {
                        let tag = it.next().expect("truncated tag");
                        let alignment = tag.alignment();
                        max_alignment = core::cmp::max(max_alignment, alignment);
                        size = round_up(size, alignment);
                        size += tag.size();
                    }
                    // Take into account any tail padding (if element(s) with largest
                    // alignment are not at the end).
                    size = round_up(size, max_alignment);
                    size
                }
                Tag::List(_) => 8,
                Tag::Array(_, num_dims) => 4 * (1 + num_dims as usize),
                Tag::Range(it) => {
                    let tag = it.clone().next().expect("truncated tag");
                    tag.size() * 3
                }
                Tag::Keyword(_) => unreachable!(),
                Tag::Object => unreachable!(),
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub struct TagIterator<'a> {
        data: &'a [u8]
    }

    impl<'a> TagIterator<'a> {
        pub fn new(data: &'a [u8]) -> TagIterator<'a> {
            TagIterator { data: data }
        }

        pub fn next(&mut self) -> Option<Tag<'a>> {
            if self.data.len() == 0 {
                return None
            }

            let tag_byte = self.data[0];
            self.data = &self.data[1..];
            Some(match tag_byte {
                b'n' => Tag::None,
                b'b' => Tag::Bool,
                b'i' => Tag::Int32,
                b'I' => Tag::Int64,
                b'f' => Tag::Float64,
                b's' => Tag::String,
                b'B' => Tag::Bytes,
                b'A' => Tag::ByteArray,
                b't' => {
                    let count = self.data[0];
                    self.data = &self.data[1..];
                    Tag::Tuple(self.sub(count), count)
                }
                b'l' => Tag::List(self.sub(1)),
                b'a' => {
                    let count = self.data[0];
                    self.data = &self.data[1..];
                    Tag::Array(self.sub(1), count)
                }
                b'r' => Tag::Range(self.sub(1)),
                b'k' => Tag::Keyword(self.sub(1)),
                b'O' => Tag::Object,
                _    => unreachable!()
            })
        }

        fn sub(&mut self, count: u8) -> TagIterator<'a> {
            let data = self.data;
            for _ in 0..count {
                self.next().expect("truncated tag");
            }
            TagIterator { data: &data[..(data.len() - self.data.len())] }
        }
    }

    impl<'a> Iterator for TagIterator<'a> {
        type Item = Tag<'a>;
        fn next(&mut self) -> Option<Self::Item> {
            (self as &mut TagIterator<'a>).next()
        }
    }

    impl<'a> fmt::Display for TagIterator<'a> {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let mut it = self.clone();
            let mut first = true;
            while let Some(tag) = it.next() {
                if first {
                    first = false
                } else {
                    write!(f, ", ")?
                }

                match tag {
                    Tag::None =>
                        write!(f, "None")?,
                    Tag::Bool =>
                        write!(f, "Bool")?,
                    Tag::Int32 =>
                        write!(f, "Int32")?,
                    Tag::Int64 =>
                        write!(f, "Int64")?,
                    Tag::Float64 =>
                        write!(f, "Float64")?,
                    Tag::String =>
                        write!(f, "String")?,
                    Tag::Bytes =>
                        write!(f, "Bytes")?,
                    Tag::ByteArray =>
                        write!(f, "ByteArray")?,
                    Tag::Tuple(it, _) => {
                        write!(f, "Tuple(")?;
                        it.fmt(f)?;
                        write!(f, ")")?;
                    }
                    Tag::List(it) => {
                        write!(f, "List(")?;
                        it.fmt(f)?;
                        write!(f, ")")?;
                    }
                    Tag::Array(it, num_dims) => {
                        write!(f, "Array(")?;
                        it.fmt(f)?;
                        write!(f, ", {})", num_dims)?;
                    }
                    Tag::Range(it) => {
                        write!(f, "Range(")?;
                        it.fmt(f)?;
                        write!(f, ")")?;
                    }
                    Tag::Keyword(it) => {
                        write!(f, "Keyword(")?;
                        it.fmt(f)?;
                        write!(f, ")")?;
                    }
                    Tag::Object =>
                        write!(f, "Object")?,
                }
            }

            Ok(())
        }
    }
}
