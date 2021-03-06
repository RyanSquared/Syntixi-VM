use std::convert::{TryFrom, TryInto};

use super::conf::{SYX_HEADER, SYX_DATA, SYX_VERSION, SYX_FORMAT, SYX_INT, SYX_NUM};

use super::object::{
    LocVar, Proto, SyxInt, SyxInteger, SyxNumber, SyxString,
    SyxType, SyxValue, Upvalue
};
use super::opcodes::{Instruction, Word};
use super::{limits, state};
use super::errors::*;

pub struct LoadState {
    input: Box<Iterator<Item = u8>>,
    name: Box<::std::fmt::Display>,
    state: Option<state::SyxState>,
}

trait Primitives {}

macro_rules! primitive {
    ($($item:ty),*) => { $(impl Primitives for $item {})* }
}

primitive!(u8, u16, u32, u64);
primitive!(i8, i16, i32, i64);
primitive!(usize, isize);
primitive!(f32, f64);

macro_rules! expand {
    ($item:ty) => {{
        (::std::mem::size_of::<$item>(), stringify!($item))
    }};
}

#[allow(dead_code)]
impl LoadState {
    pub fn from_read(
        mut input: impl ::std::io::Read,
        name: impl Into<String>,
    ) -> Result<Proto> {
        let mut buffer: Vec<u8> = Vec::new();
        let into_name = name.into();
        if input.read_to_end(&mut buffer).is_ok() {
            LoadState::from_u8(buffer, into_name.clone())
        } else {
            Err(ErrorKind::BufferNotReadable(into_name).into())
        }
    }

    pub fn from_u8(buffer: Vec<u8>, name: impl Into<String>)
        -> Result<Proto>
    {
        let mut state = LoadState {
            input: Box::new(buffer.into_iter()),
            name: Box::new(name.into()),
            state: None,
        };
        let proto = state.load_chunk(state::SyxState::new())?;
        match state.load::<u8>() {
            Err(_) => Ok(proto),
            Ok(_) => Err(ErrorKind::BufferNotEmpty.into()),
        }
    }

    fn assert_verification(&mut self, val: bool, err: impl ::std::fmt::Display)
        -> Result<()>
    {
        return if !val {
            self.raise_from_verification(err)
        } else {
            Ok(())
        }
    }

    fn raise_from_verification(&mut self, err: impl ::std::fmt::Display)
        -> Result<()>
    {
        Err(ErrorKind::InvalidVerification(self.name.to_string(),
                                           err.to_string()).into())
    }

    fn load_range(&mut self, range: usize) -> Result<Vec<u8>> {
        let v: Vec<u8> = self.input.by_ref().take(range).collect();
        self.assert_verification(v.len() == range,
                                 format!("Not enough bytes: {}", range))?;
        Ok(v)
        // made redundant by the above
        /*
        let mut ret: Vec<u8> = Vec::with_capacity(range);
        for i in 0..range {
            if let Some(mut ch) = self.input.next() {
                ret.push(ch);
            } else {
                self.raise_from_verification(
                    format!("Missing byte at pos: {}", i))?;
            }
        };
        Ok(ret)
        */
    }

    fn load<T: Copy + Primitives>(&mut self) -> Result<T> {
        /*
         * Safety of this method
         * ---
         * I had to mark unsafe because of the transmutation, but this is why
         * it will alwasy pass:
         *
         * 1. It will always transmute bytes directly to the size of T
         * 2. The size of T is loaded from self.load_range, which either grabs
         *    the whole thing or fails to load
         * 3. All values of type `Primitives` are defined at the top of this
         *    file and will always be Rust primitives.
         */
        // ::TODO:: optimize for <u8> when specializations lands:
        // https://github.com/rust-lang/rust/issues/31844
        // https://github.com/rust-lang/rfcs/blob/master/text/1210-impl-specialization.md
        let size = ::std::mem::size_of::<T>();
        let bytes = self.load_range(size)?;
        Ok(unsafe { *(&bytes[0] as *const u8 as *const T) })
    }

    fn load_string(&mut self) -> Result<SyxString> {
        let mut size: usize = self.load::<u8>()? as usize;
        if size == 0xFF {
            size = self.load::<usize>()?;
        }
        if size == 0 {
            // Turns out it can happen with stripped debug info. We'll just
            // return an empty string as it's not likely to be empty if it does
            // exist - wait, what happens in PUC-Rio Lua?..
            Ok(vec![])
        } else {
            // So, Lua has a concept of "short" and "long" strings. This can be
            // optimized later in the future, as well as the SyxString type, to
            // include a hash field.
            size -= 1;
            if size < limits::SYX_MAXSHORTLEN {
                self.load_range(size)
            } else {
                self.load_range(size)
            }
        }
    }

    fn load_constants(&mut self, proto: &mut Proto) -> Result<()> {
        let constant_count: isize = self.load::<i32>()? as isize;
        proto.constants.clear();
        for _ in 0..constant_count {
            // get type from byte
            proto.constants.push(match SyxType::try_from(self.load::<u8>()?)? {
                SyxType::TNIL => SyxValue::Nil,
                SyxType::TBOOLEAN => SyxValue::Bool(self.load::<u8>()? == 1),
                // these lines represent everything wrong with the world
                // they take up more than 80 characters
                SyxType::TNUMFLT => SyxValue::Number(self.load::<SyxNumber>()?),
                SyxType::TNUMINT => SyxValue::Integer(self.load::<SyxInteger>()?),
                | SyxType::TSHRSTR
                | SyxType::TLNGSTR => SyxValue::String(self.load_string()?),
                x => {
                    return Err(ErrorKind::InvalidConstantType(x).into());
                }
            });
        }
        Ok(())
    }

    fn load_code(&mut self, proto: &mut Proto) -> Result<()> {
        let count = self.load::<SyxInt>()?;
        proto.instructions.clear();
        proto.instructions.reserve(count as usize);
        for _ in 0..(count) {
            proto.instructions.push(self.load::<Word>()?.try_into()?);
        }
        Ok(())
    }

    fn load_protos(&mut self, proto: &mut Proto) -> Result<()> {
        let count = self.load::<SyxInt>()?;
        proto.protos.clear();
        proto.protos.reserve(count as usize);
        for _ in 0..(count) {
            let mut new_proto = Proto::new();
            self.load_function(&mut new_proto, vec![])?;
            proto.protos.push(new_proto);
        }
        Ok(())
    }

    fn load_upvalues(&mut self, proto: &mut Proto) -> Result<()> {
        let upvalues_count = self.load::<SyxInt>()?;
        proto.upvalues.clear();
        proto.upvalues.reserve(upvalues_count as usize);
        for _ in 0..upvalues_count {
            proto.upvalues.push(Upvalue {
                name: vec![],
                instack: self.load::<u8>()?,
                idx: self.load::<u8>()?,
            })
        }
        Ok(())
    }

    fn load_debug(&mut self, proto: &mut Proto) -> Result<()> {
        let lines = self.load::<SyxInt>()? as usize;
        proto.lineinfo.clear();
        proto.lineinfo.reserve(lines);
        for _ in 0..lines {
            proto.lineinfo.push(self.load::<SyxInt>()?);
        }
        let size = self.load::<SyxInt>()? as usize;
        proto.locvars.clear();
        proto.locvars.reserve(size);
        // load locvars
        for _ in 0..size {
            proto.locvars.push(LocVar {
                varname: self.load_string()?,
                startpc: self.load::<SyxInt>()?,
                endpc: self.load::<SyxInt>()?,
            });
        }
        // end trash
        let upvalue_count = self.load::<SyxInt>()? as usize;
        for i in 0..upvalue_count {
            match proto.upvalues.get_mut(i) {
                Some(value) => value.name = self.load_string()?,
                None => return Err(ErrorKind::InvalidUpvalueIndex(i).into()),
            }
        }
        Ok(())
    }

    fn load_function(&mut self, proto: &mut Proto, source: SyxString)
        -> Result<()>
    {
        let loaded_source = self.load_string()?;
        proto.source = String::from_utf8({
            if !loaded_source.is_empty() {
                loaded_source
            } else {
                source
            }
        }).chain_err(|| ErrorKind::InvalidSourceName)?;
        proto.linedefined = self.load::<SyxInt>()?;
        proto.lastlinedefined = self.load::<SyxInt>()?;
        proto.numparams = self.load::<u8>()?;
        proto.is_vararg = self.load::<u8>()? != 0;
        proto.maxstacksize = self.load::<u8>()?;
        self.load_code(proto)?;
        self.load_constants(proto)?;
        self.load_upvalues(proto)?;
        self.load_protos(proto)?;
        self.load_debug(proto)?;
        Ok(())
    }

    fn check_size(&mut self, size: (usize, &'static str)) -> Result<()> {
        if let Ok(bytecode_size) = self.load::<u8>() {
            self.assert_verification(
                bytecode_size == (size.0 as u8),
                format!("size mismatch: {}", size.1),
            )
        } else {
            Ok(())
        }
    }

    fn check_literal(
        &mut self,
        value_impl: impl Into<Vec<u8>>,
        err: impl ::std::fmt::Display,
    ) -> Result<()> {
        let value = value_impl.into();
        if let Ok(literal) = self.load_range(value.len()) {
            self.assert_verification(literal == value,
                                     format!("literal mismatch: {}", err))
        } else {
            Ok(())
        }
    }

    fn check_header(&mut self) -> Result<()> {
        self.check_literal(SYX_HEADER, "header")?;
        let bt = self.load::<u8>()?;
        self.assert_verification(bt == SYX_VERSION, "version mismatch")?;
        let bt = self.load::<u8>()?;
        self.assert_verification(bt == SYX_FORMAT, "format mismatch")?;
        self.check_literal(SYX_DATA, "load order verification")?;
        self.check_size(expand!(i32))?;
        self.check_size(expand!(usize))?;
        self.check_size(expand!(Word))?;
        self.check_size(expand!(SyxInteger))?;
        self.check_size(expand!(SyxNumber))?;
        let int: SyxInteger = self.load::<SyxInteger>()?;
        self.assert_verification(int == SYX_INT, "endianness mismatch")?;
        let float: SyxNumber = self.load::<SyxNumber>()?;
        self.assert_verification(float == SYX_NUM, "float format mismatch")?;
        Ok(())
    }

    fn load_chunk(&mut self, _lstate: state::SyxState) -> Result<Proto> {
        self.state = Some(state::SyxState {});
        // ::TODO:: ::XXX:: here is where i left off
        // cl->p
        self.check_header()?;
        let mut proto = Proto::new();
        let _upvals = self.load::<u8>()?;
        self.load_function(&mut proto, vec![])?;
        Ok(proto)
    }
}
