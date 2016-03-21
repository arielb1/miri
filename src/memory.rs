use byteorder::{self, ByteOrder, NativeEndian, ReadBytesExt, WriteBytesExt};
use std::collections::{BTreeMap, HashMap};
use std::collections::Bound::{Included, Excluded};
use std::mem;
use std::ptr;

use error::{EvalError, EvalResult};
use primval::PrimVal;

pub struct Memory {
    alloc_map: HashMap<u64, Allocation>,
    next_id: u64,
    pub pointer_size: usize,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AllocId(u64);

#[derive(Debug)]
pub struct Allocation {
    pub bytes: Vec<u8>,
    pub relocations: BTreeMap<usize, AllocId>,
    // TODO(tsion): undef mask
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Pointer {
    pub alloc_id: AllocId,
    pub offset: usize,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FieldRepr {
    pub offset: usize,
    pub size: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Repr {
    /// Representation for a non-aggregate type such as a boolean, integer, character or pointer.
    Primitive {
        size: usize
    },

    /// The representation for aggregate types including structs, enums, and tuples.
    Aggregate {
        /// The size of the discriminant (an integer). Should be between 0 and 8. Always 0 for
        /// structs and tuples.
        discr_size: usize,

        /// The size of the entire aggregate, including the discriminant.
        size: usize,

        /// The representations of the contents of each variant.
        variants: Vec<Vec<FieldRepr>>,
    },

    Array {
        elem_size: usize,

        /// Number of elements.
        length: usize,
    },
}

impl Memory {
    pub fn new() -> Self {
        Memory {
            alloc_map: HashMap::new(),
            next_id: 0,

            // TODO(tsion): Should this be host's or target's usize?
            pointer_size: mem::size_of::<usize>(),
        }
    }

    pub fn allocate(&mut self, size: usize) -> Pointer {
        let id = AllocId(self.next_id);
        let alloc = Allocation { bytes: vec![0; size], relocations: BTreeMap::new() };
        self.alloc_map.insert(self.next_id, alloc);
        self.next_id += 1;
        Pointer {
            alloc_id: id,
            offset: 0,
        }
    }

    pub fn get(&self, id: AllocId) -> EvalResult<&Allocation> {
        self.alloc_map.get(&id.0).ok_or(EvalError::DanglingPointerDeref)
    }

    pub fn get_mut(&mut self, id: AllocId) -> EvalResult<&mut Allocation> {
        self.alloc_map.get_mut(&id.0).ok_or(EvalError::DanglingPointerDeref)
    }

    fn get_bytes(&self, ptr: Pointer, size: usize) -> EvalResult<&[u8]> {
        let alloc = try!(self.get(ptr.alloc_id));
        try!(alloc.check_no_relocations(ptr.offset, ptr.offset + size));
        Ok(&alloc.bytes[ptr.offset..ptr.offset + size])
    }

    fn get_bytes_mut(&mut self, ptr: Pointer, size: usize) -> EvalResult<&mut [u8]> {
        let alloc = try!(self.get_mut(ptr.alloc_id));
        try!(alloc.check_no_relocations(ptr.offset, ptr.offset + size));
        Ok(&mut alloc.bytes[ptr.offset..ptr.offset + size])
    }

    pub fn copy(&mut self, src: Pointer, dest: Pointer, size: usize) -> EvalResult<()> {
        let (src_bytes, mut relocations) = {
            let alloc = try!(self.get_mut(src.alloc_id));
            try!(alloc.check_relocation_edges(src.offset, src.offset + size));
            let bytes = alloc.bytes[src.offset..src.offset + size].as_mut_ptr();

            let relocations: Vec<(usize, AllocId)> = alloc.relocations
                .range(Included(&src.offset), Excluded(&(src.offset + size)))
                .map(|(&k, &v)| (k, v))
                .collect();

            (bytes, relocations)
        };

        // Update relocation offsets for the new positions in the destination allocation.
        for &mut (ref mut offset, _) in &mut relocations {
            *offset += dest.offset;
            *offset -= src.offset;
        }

        let dest_bytes = try!(self.get_bytes_mut(dest, size)).as_mut_ptr();

        // TODO(tsion): Clear the destination range's existing relocations.
        try!(self.get_mut(dest.alloc_id)).relocations.extend(relocations);

        // SAFE: The above indexing would have panicked if there weren't at least `size` bytes
        // behind `src` and `dest`. Also, we use the overlapping-safe `ptr::copy` if `src` and
        // `dest` could possibly overlap.
        unsafe {
            if src.alloc_id == dest.alloc_id {
                ptr::copy(src_bytes, dest_bytes, size);
            } else {
                ptr::copy_nonoverlapping(src_bytes, dest_bytes, size);
            }
        }

        Ok(())
    }

    pub fn write_bytes(&mut self, ptr: Pointer, src: &[u8]) -> EvalResult<()> {
        self.get_bytes_mut(ptr, src.len()).map(|dest| dest.clone_from_slice(src))
    }

    pub fn read_ptr(&self, ptr: Pointer) -> EvalResult<Pointer> {
        let alloc = try!(self.get(ptr.alloc_id));
        try!(alloc.check_relocation_edges(ptr.offset, ptr.offset + self.pointer_size));
        let bytes = &alloc.bytes[ptr.offset..ptr.offset + self.pointer_size];
        let offset = byteorder::NativeEndian::read_u64(bytes) as usize;

        match alloc.relocations.get(&ptr.offset) {
            Some(&alloc_id) => Ok(Pointer { alloc_id: alloc_id, offset: offset }),
            None => Err(EvalError::ReadBytesAsPointer),
        }
    }

    // TODO(tsion): Detect invalid writes here and elsewhere.
    pub fn write_ptr(&mut self, dest: Pointer, ptr_val: Pointer) -> EvalResult<()> {
        {
            let size = self.pointer_size;
            let bytes = try!(self.get_bytes_mut(dest, size));
            byteorder::NativeEndian::write_u64(bytes, ptr_val.offset as u64);
        }
        let alloc = try!(self.get_mut(dest.alloc_id));
        alloc.relocations.insert(dest.offset, ptr_val.alloc_id);
        Ok(())
    }

    pub fn write_primval(&mut self, ptr: Pointer, val: PrimVal) -> EvalResult<()> {
        match val {
            PrimVal::Bool(b) => self.write_bool(ptr, b),
            PrimVal::I8(n)   => self.write_int(ptr, n as i64, 1),
            PrimVal::I16(n)  => self.write_int(ptr, n as i64, 2),
            PrimVal::I32(n)  => self.write_int(ptr, n as i64, 4),
            PrimVal::I64(n)  => self.write_int(ptr, n as i64, 8),
            PrimVal::U8(n)   => self.write_uint(ptr, n as u64, 1),
            PrimVal::U16(n)  => self.write_uint(ptr, n as u64, 2),
            PrimVal::U32(n)  => self.write_uint(ptr, n as u64, 4),
            PrimVal::U64(n) | PrimVal::IntegerPtr(n) => self.write_uint(ptr, n as u64, 8),
            PrimVal::AbstractPtr(_p) => unimplemented!(),
        }
    }

    pub fn read_bool(&self, ptr: Pointer) -> EvalResult<bool> {
        let bytes = try!(self.get_bytes(ptr, 1));
        match bytes[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(EvalError::InvalidBool),
        }
    }

    pub fn write_bool(&mut self, ptr: Pointer, b: bool) -> EvalResult<()> {
        self.get_bytes_mut(ptr, 1).map(|bytes| bytes[0] = b as u8)
    }

    pub fn read_int(&self, ptr: Pointer, size: usize) -> EvalResult<i64> {
        self.get_bytes(ptr, size).map(|mut b| b.read_int::<NativeEndian>(size).unwrap())
    }

    pub fn write_int(&mut self, ptr: Pointer, n: i64, size: usize) -> EvalResult<()> {
        self.get_bytes_mut(ptr, size).map(|mut b| b.write_int::<NativeEndian>(n, size).unwrap())
    }

    pub fn read_uint(&self, ptr: Pointer, size: usize) -> EvalResult<u64> {
        self.get_bytes(ptr, size).map(|mut b| b.read_uint::<NativeEndian>(size).unwrap())
    }

    pub fn write_uint(&mut self, ptr: Pointer, n: u64, size: usize) -> EvalResult<()> {
        self.get_bytes_mut(ptr, size).map(|mut b| b.write_uint::<NativeEndian>(n, size).unwrap())
    }

    pub fn read_isize(&self, ptr: Pointer) -> EvalResult<i64> {
        self.read_int(ptr, self.pointer_size)
    }

    pub fn write_isize(&mut self, ptr: Pointer, n: i64) -> EvalResult<()> {
        let size = self.pointer_size;
        self.write_int(ptr, n, size)
    }

    pub fn read_usize(&self, ptr: Pointer) -> EvalResult<u64> {
        self.read_uint(ptr, self.pointer_size)
    }

    pub fn write_usize(&mut self, ptr: Pointer, n: u64) -> EvalResult<()> {
        let size = self.pointer_size;
        self.write_uint(ptr, n, size)
    }
}

impl Allocation {
    fn check_bounds(&self, start: usize, end: usize) -> EvalResult<()> {
        if start <= self.bytes.len() && end <= self.bytes.len() {
            Ok(())
        } else {
            Err(EvalError::PointerOutOfBounds)
        }
    }

    fn count_overlapping_relocations(&self, start: usize, end: usize) -> usize {
        self.relocations.range(
            // FIXME(tsion): Assuming pointer size is 8. Move this method to Memory.
            Included(&start.saturating_sub(8 - 1)),
            Excluded(&end)
        ).count()
    }

    fn check_relocation_edges(&self, start: usize, end: usize) -> EvalResult<()> {
        try!(self.check_bounds(start, end));
        let n =
            self.count_overlapping_relocations(start, start) +
            self.count_overlapping_relocations(end, end);
        if n == 0 {
            Ok(())
        } else {
            Err(EvalError::ReadPointerAsBytes)
        }
    }

    fn check_no_relocations(&self, start: usize, end: usize) -> EvalResult<()> {
        try!(self.check_bounds(start, end));
        if self.count_overlapping_relocations(start, end) == 0 {
            Ok(())
        } else {
            Err(EvalError::ReadPointerAsBytes)
        }
    }
}

impl Pointer {
    pub fn offset(self, i: isize) -> Self {
        Pointer { offset: (self.offset as isize + i) as usize, ..self }
    }
}

impl Repr {
    pub fn size(&self) -> usize {
        match *self {
            Repr::Primitive { size } => size,
            Repr::Aggregate { size, .. } => size,
            Repr::Array { elem_size, length } => elem_size * length,
        }
    }
}
