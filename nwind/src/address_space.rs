use std::mem;
use std::sync::Arc;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::fmt;
use std::borrow::Cow;

use byteorder::{self, ByteOrder};
use cpp_demangle;

use arch::{Architecture, Registers, Endianity};
use dwarf_regs::DwarfRegs;
use maps::Region;
use range_map::RangeMap;
use unwind_context::UnwindContext;
use binary::{BinaryData, LoadHeader};
use symbols::Symbols;
use frame_descriptions::{FrameDescriptions, ContextCache, UnwindInfo, AddressMapping};
use types::{Bitness, Inode, UserFrame, Endianness, BinaryId};

fn translate_address( mappings: &[AddressMapping], address: u64 ) -> u64 {
    if let Some( mapping ) = mappings.iter().find( |mapping| address >= mapping.actual_address && address < (mapping.actual_address + mapping.size) ) {
        address - mapping.actual_address + mapping.declared_address
    } else {
        address
    }
}

#[derive(Clone, PartialEq, Eq, Default, Debug, Hash)]
struct BinaryAddresses {
    arm_exidx: Option< u64 >,
    arm_extab: Option< u64 >
}

pub struct Binary< A: Architecture > {
    name: String,
    virtual_addresses: BinaryAddresses,
    load_headers: Vec< LoadHeader >,
    mappings: Vec< AddressMapping >,
    data: Option< Arc< BinaryData > >,
    symbols: Vec< Symbols >,
    frame_descriptions: Option< FrameDescriptions< A::Endianity > >
}

pub type BinaryHandle< A > = Arc< Binary< A > >;

pub fn lookup_binary< 'a, A: Architecture, M: MemoryReader< A > >( nth_frame: usize, memory: &'a M, regs: &A::Regs ) -> Option< &'a BinaryHandle< A > > {
    let address = A::get_instruction_pointer( regs ).unwrap();
    let region = match memory.get_region_at_address( address ) {
        Some( region ) => region,
        None => {
            debug!( "Cannot find a binary corresponding to address 0x{:016X}", address );
            return None;
        }
    };

    debug!(
        "Frame #{}: '{}' at 0x{:016X} (0x{:X}): {:?}",
        nth_frame,
        region.binary().name(),
        address,
        translate_address( &region.binary().mappings, address ),
        region.binary().decode_symbol_once( address )
    );

    Some( &region.binary() )
}

impl< A: Architecture > Binary< A > {
    #[inline]
    pub fn name( &self ) -> &str {
        &self.name
    }

    #[inline]
    pub fn data( &self ) -> Option< &Arc< BinaryData > > {
        self.data.as_ref()
    }

    pub fn lookup_unwind_row< 'a >( &'a self, ctx_cache: &'a mut ContextCache< A::Endianity >, address: u64 ) -> Option< UnwindInfo< 'a, A::Endianity > > {
        if let Some( ref frame_descriptions ) = self.frame_descriptions {
            frame_descriptions.find_unwind_info( ctx_cache, &self.mappings, address )
        } else {
            None
        }
    }

    pub fn arm_exidx_address( &self ) -> Option< u64 > {
        self.virtual_addresses.arm_exidx
    }

    pub fn arm_extab_address( &self ) -> Option< u64 > {
        self.virtual_addresses.arm_extab
    }

    pub fn decode_symbol_while< 'a >( &'a self, address: u64, callback: &mut FnMut( &mut Frame< 'a > ) -> bool ) {
        let relative_address = translate_address( &self.mappings, address );
        let mut frame = Frame {
            absolute_address: address,
            relative_address,
            name: None,
            demangled_name: None,
            file: None,
            line: None,
            column: None,
            is_inline: false
        };

        for symbols in &self.symbols {
            if let Some( (_, symbol) ) = symbols.get_symbol( relative_address ) {
                let demangled_name =
                    cpp_demangle::Symbol::new( symbol ).ok()
                        .and_then( |symbol| {
                            symbol.demangle( &cpp_demangle::DemangleOptions { no_params: false } ).ok()
                    });

                frame.name = Some( symbol.into() );
                frame.demangled_name = demangled_name.map( |symbol| symbol.into() );
                break;
            }
        }

        callback( &mut frame );
    }

    fn decode_symbol_once( &self, address: u64 ) -> Frame {
        let mut output = Frame {
            absolute_address: address,
            relative_address: address,
            name: None,
            demangled_name: None,
            file: None,
            line: None,
            column: None,
            is_inline: false
        };

        self.decode_symbol_while( address, &mut |frame| {
            mem::swap( &mut output, frame );
            false
        });

        output
    }
}

pub struct BinaryRegion< A: Architecture > {
    binary: BinaryHandle< A >,
    memory_region: Region
}

impl< A: Architecture > BinaryRegion< A > {
    #[inline]
    pub fn binary( &self ) -> &BinaryHandle< A > {
        &self.binary
    }

    #[inline]
    pub fn is_executable( &self ) -> bool {
        self.memory_region.is_executable
    }

    #[inline]
    pub fn file_offset( &self ) -> u64 {
        self.memory_region.file_offset
    }
}

struct Memory< 'a, A: Architecture + 'a, T: ?Sized + BufferReader + 'a > {
    regions: &'a RangeMap< BinaryRegion< A > >,
    stack_address: u64,
    stack: &'a T
}

impl< 'a, A: Architecture, T: ?Sized + BufferReader + 'a > Memory< 'a, A, T > {
    #[inline]
    fn get_value_at_address< V: Primitive >( &self, endianness: Endianness, address: u64 ) -> Option< V > {
        if address >= self.stack_address {
            let offset = address - self.stack_address;
            if let Some( value ) = V::read_from_buffer( self.stack, endianness, offset ) {
                debug!( "Read stack address 0x{:016X} (+{}): 0x{:016X}", address, offset, value );
                return Some( value );
            }
        }

        if let Some( (range, region) ) = self.regions.get( address ) {
            let offset = (region.file_offset() + (address - range.start)) as usize;
            debug!( "Reading from binary '{}' at address 0x{:016X} (+{})", region.binary().name(), address, offset );
            let slice = &region.binary().data()?.as_bytes()[ offset..offset + mem::size_of::< V >() ];
            let value = V::read_from_slice( endianness, slice );
            Some( value )
        } else {
            None
        }
    }
}

impl< 'a, A: Architecture, T: ?Sized + BufferReader + 'a > MemoryReader< A > for Memory< 'a, A, T > {
    #[inline]
    fn get_region_at_address( &self, address: u64 ) -> Option< &BinaryRegion< A > > {
        self.regions.get_value( address )
    }

    #[inline]
    fn get_u32_at_address( &self, endianness: Endianness, address: u64 ) -> Option< u32 > {
        self.get_value_at_address::< u32 >( endianness, address )
    }

    #[inline]
    fn get_u64_at_address( &self, endianness: Endianness, address: u64 ) -> Option< u64 > {
        self.get_value_at_address::< u64 >( endianness, address )
    }

    #[inline]
    fn is_stack_address( &self, address: u64 ) -> bool {
        address >= self.stack_address && address < (self.stack_address + self.stack.len() as u64)
    }
}

pub trait BufferReader {
    fn len( &self ) -> usize;
    fn get_u32_at_offset( &self, endianness: Endianness, offset: u64 ) -> Option< u32 >;
    fn get_u64_at_offset( &self, endianness: Endianness, offset: u64 ) -> Option< u64 >;
}

pub trait MemoryReader< A: Architecture > {
    fn get_region_at_address( &self, address: u64 ) -> Option< &BinaryRegion< A > >;
    fn get_u32_at_address( &self, endianness: Endianness, address: u64 ) -> Option< u32 >;
    fn get_u64_at_address( &self, endianness: Endianness, address: u64 ) -> Option< u64 >;
    fn is_stack_address( &self, address: u64 ) -> bool;

    #[inline]
    fn get_pointer_at_address( &self, endianness: Endianness, bitness: Bitness, address: u64 ) -> Option< u64 > {
        match bitness {
            Bitness::B32 => self.get_u32_at_address( endianness, address ).map( |value| value as u64 ),
            Bitness::B64 => self.get_u64_at_address( endianness, address )
        }
    }
}

pub trait Primitive: Sized + fmt::UpperHex {
    fn read_from_slice( endianness: Endianness, slice: &[u8] ) -> Self;
    fn read_from_buffer< T: ?Sized + BufferReader >( buffer: &T, endianness: Endianness, offset: u64 ) -> Option< Self >;
}

impl Primitive for u32 {
    #[inline]
    fn read_from_slice( endianness: Endianness, slice: &[u8] ) -> Self {
        match endianness {
            Endianness::LittleEndian => byteorder::LittleEndian::read_u32( slice ),
            Endianness::BigEndian => byteorder::BigEndian::read_u32( slice ),
        }
    }

    #[inline]
    fn read_from_buffer< T: ?Sized + BufferReader >( buffer: &T, endianness: Endianness, offset: u64 ) -> Option< Self > {
        buffer.get_u32_at_offset( endianness, offset )
    }
}

impl Primitive for u64 {
    #[inline]
    fn read_from_slice( endianness: Endianness, slice: &[u8] ) -> Self {
        match endianness {
            Endianness::LittleEndian => byteorder::LittleEndian::read_u64( slice ),
            Endianness::BigEndian => byteorder::BigEndian::read_u64( slice ),
        }
    }

    #[inline]
    fn read_from_buffer< T: ?Sized + BufferReader >( buffer: &T, endianness: Endianness, offset: u64 ) -> Option< Self > {
        buffer.get_u64_at_offset( endianness, offset )
    }
}

fn contains( range: Range< u64 >, value: u64 ) -> bool {
    (range.start <= value) && (value < range.end)
}

fn calculate_virtual_addr( region: &Region, physical_section_offset: u64 ) -> Option< u64 > {
    let region_range = region.file_offset..region.file_offset + (region.end - region.start);
    if contains( region_range, physical_section_offset ) {
        let virtual_addr = region.start + physical_section_offset - region.file_offset;
        Some( virtual_addr )
    } else {
        None
    }
}

pub struct LoadHandle {
    binary: Option< Arc< BinaryData > >,
    debug_binary: Option< Arc< BinaryData > >,
    symbols: Vec< Symbols >,
    mappings: Vec< LoadHeader >,
    load_frame_descriptions: bool,
    load_symbols: bool
}

impl LoadHandle {
    pub fn set_binary( &mut self, data: Arc< BinaryData > ) {
        self.binary = Some( data );
    }

    pub fn set_debug_binary( &mut self, data: Arc< BinaryData > ) {
        self.debug_binary = Some( data );
    }

    pub fn add_symbols( &mut self, symbols: Symbols ) {
        self.symbols.push( symbols );
    }

    pub fn add_region_mapping( &mut self, mapping: LoadHeader ) {
        self.mappings.push( mapping );
    }

    pub fn should_load_frame_descriptions( &mut self, value: bool ) {
        self.load_frame_descriptions = value;
    }

    pub fn should_load_symbols( &mut self, value: bool ) {
        self.load_symbols = value;
    }

    fn is_empty( &self ) -> bool {
        self.binary.is_none() &&
        self.mappings.is_empty()
    }
}

#[derive(Debug)]
pub struct Frame< 'a > {
    pub absolute_address: u64,
    pub relative_address: u64,
    pub name: Option< Cow< 'a, str > >,
    pub demangled_name: Option< Cow< 'a, str > >,
    pub file: Option< String >,
    pub line: Option< u64 >,
    pub column: Option< u64 >,
    pub is_inline: bool
}

pub trait IAddressSpace {
    fn reload( &mut self, regions: Vec< Region >, try_load: &mut FnMut( &Region, &mut LoadHandle ) ) -> Reloaded;
    fn unwind( &mut self, regs: &mut DwarfRegs, stack: &BufferReader, output: &mut Vec< UserFrame > );
    fn decode_symbol_while< 'a >( &'a self, address: u64, callback: &mut FnMut( &mut Frame< 'a > ) -> bool );
    fn decode_symbol_once( &self, address: u64 ) -> Frame;
    fn set_panic_on_partial_backtrace( &mut self, value: bool );
}

#[derive(Clone, Default)]
pub struct Reloaded {
    pub binaries_unmapped: Vec< (Option< Inode >, String) >,
    pub binaries_mapped: Vec< (Option< Inode >, String, Option< Arc< BinaryData > >) >,
    pub regions_unmapped: Vec< Range< u64 > >,
    pub regions_mapped: Vec< Region >
}

pub struct AddressSpace< A: Architecture > {
    pub(crate) ctx: UnwindContext< A >,
    pub(crate) regions: RangeMap< BinaryRegion< A > >,
    binary_map: HashMap< BinaryId, BinaryHandle< A > >,
    panic_on_partial_backtrace: bool
}

impl< A: Architecture > IAddressSpace for AddressSpace< A > {
    fn reload( &mut self, regions: Vec< Region >, try_load: &mut FnMut( &Region, &mut LoadHandle ) ) -> Reloaded {
        debug!( "Reloading..." );

        struct Data< E: Endianity > {
            name: String,
            binary_data: Option< Arc< BinaryData > >,
            addresses: BinaryAddresses,
            load_headers: Vec< LoadHeader >,
            mappings: Vec< AddressMapping >,
            symbols: Vec< Symbols >,
            frame_descriptions: Option< FrameDescriptions< E > >,
            regions: Vec< (Region, bool) >,
            load_symbols: bool,
            load_frame_descriptions: bool,
            is_old: bool
        }

        let mut reloaded = Reloaded::default();

        let mut old_binary_map = HashMap::new();
        mem::swap( &mut old_binary_map, &mut self.binary_map );

        let mut old_region_map = RangeMap::new();
        mem::swap( &mut old_region_map, &mut self.regions );

        let mut old_regions = HashSet::new();
        for (_, region) in old_region_map {
            old_regions.insert( region.memory_region );
        }
        let old_region_count = old_regions.len();

        let mut new_binary_map = HashMap::new();
        let mut tried_to_load = HashSet::new();
        for region in regions {
            if region.is_shared || region.name.is_empty() || (region.inode == 0 && region.name != "[vdso]") {
                continue;
            }

            debug!( "Adding memory region at 0x{:016X}-0x{:016X} for '{}' with offset 0x{:08X}", region.start, region.end, region.name, region.file_offset );
            let id: BinaryId = (&region).into();

            if !new_binary_map.contains_key( &id ) {
                if let Some( binary ) = old_binary_map.remove( &id ) {
                    let (binary_data, symbols, frame_descriptions, load_headers) = match Arc::try_unwrap( binary ) {
                        Ok( binary ) => (binary.data, binary.symbols, binary.frame_descriptions, binary.load_headers),
                        Err( _ ) => {
                            unimplemented!();
                        }
                    };

                    new_binary_map.insert( id.clone(), Data {
                        name: region.name.clone(),
                        binary_data,
                        addresses: BinaryAddresses::default(),
                        load_headers,
                        mappings: Default::default(),
                        symbols,
                        frame_descriptions,
                        regions: Vec::new(),
                        load_symbols: false,
                        load_frame_descriptions: false,
                        is_old: true
                    });
                } else if !tried_to_load.contains( &id ) {
                    tried_to_load.insert( id.clone() );

                    let mut handle = LoadHandle {
                        binary: None,
                        debug_binary: None,
                        symbols: Vec::new(),
                        mappings: Vec::new(),
                        load_frame_descriptions: true,
                        load_symbols: true
                    };

                    try_load( &region, &mut handle );
                    if handle.is_empty() {
                        continue;
                    }

                    if let Some( binary_data ) = handle.binary.as_ref() {
                        handle.mappings = binary_data.load_headers().into();
                    }

                    new_binary_map.insert( id.clone(), Data {
                        name: region.name.clone(),
                        binary_data: handle.binary,
                        addresses: BinaryAddresses::default(),
                        load_headers: handle.mappings,
                        mappings: Default::default(),
                        symbols: handle.symbols,
                        frame_descriptions: None,
                        regions: Vec::new(),
                        load_symbols: handle.load_symbols,
                        load_frame_descriptions: handle.load_frame_descriptions,
                        is_old: false
                    });
                } else {
                    continue;
                }
            }

            let is_new = !old_regions.remove( &region );
            let mut data = new_binary_map.get_mut( &id ).unwrap();

            if region.file_offset == 0 {
                if let Some( load_header ) = data.load_headers.iter().find( |header| header.file_offset == 0 ) {
                    let base_address = region.start.wrapping_sub( load_header.address );
                    debug!( "'{}': found base address at 0x{:016X}", region.name, base_address );
                }
            }

            if let Some( header ) = data.load_headers.iter().find( |header| header.file_offset == region.file_offset ) {
                data.mappings.push( AddressMapping {
                    declared_address: header.address,
                    actual_address: region.start,
                    file_offset: header.file_offset,
                    size: region.end - region.start
                });
            }

            macro_rules! section {
                ($name:expr, $section_range_getter:ident, $output_addr:expr) => {
                    if $output_addr.is_none() {
                        if let Some( binary_data ) = data.binary_data.as_ref() {
                            if let Some( section_range ) = binary_data.$section_range_getter() {
                                if let Some( addr ) = calculate_virtual_addr( &region, section_range.start as u64 ) {
                                    debug!( "'{}': found {} section at 0x{:016X} (+0x{:08X})", region.name, $name, addr, addr - region.start );
                                    *$output_addr = Some( addr );
                                }
                            }
                        }
                    }
                }
            }

            section!( ".ARM.exidx", arm_exidx_range, &mut data.addresses.arm_exidx );
            section!( ".ARM.extab", arm_extab_range, &mut data.addresses.arm_extab );

            data.regions.push( (region, is_new) );
        }

        let mut new_regions = Vec::new();
        for (id, data) in new_binary_map {
            if !data.is_old {
                reloaded.binaries_mapped.push( (id.to_inode(), data.name.clone(), data.binary_data.clone()) );
            }

            let mut symbols = data.symbols;
            if data.load_symbols {
                if symbols.is_empty() {
                    if let Some( binary_data ) = data.binary_data.as_ref() {
                        symbols.push( Symbols::load_from_binary_data( &binary_data ) );
                    }
                }
            }

            let frame_descriptions = match data.frame_descriptions {
                Some( frame_descriptions ) => Some( frame_descriptions ),
                None if data.load_frame_descriptions => {
                    if let Some( binary_data ) = data.binary_data.as_ref() {
                        FrameDescriptions::load( &binary_data )
                    } else {
                        None
                    }
                },
                None => None
            };

            let binary = Arc::new( Binary {
                name: data.name,
                data: data.binary_data,
                virtual_addresses: data.addresses,
                load_headers: data.load_headers,
                mappings: data.mappings,
                symbols,
                frame_descriptions
            });

            for (region, is_new) in data.regions {
                let start = region.start;
                let end = region.end;
                let binary_region = BinaryRegion {
                    binary: binary.clone(),
                    memory_region: region.clone()
                };

                if is_new {
                    reloaded.regions_mapped.push( region );
                }

                new_regions.push( ((start..end), binary_region) );
            }

            self.binary_map.insert( id, binary );
        }

        let new_region_count = new_regions.len();
        self.regions = RangeMap::from_vec( new_regions );
        assert_eq!( new_region_count, self.regions.len() );

        for (id, binary) in old_binary_map {
            reloaded.binaries_unmapped.push( (id.to_inode(), binary.name.clone()) );
        }

        reloaded.regions_unmapped.extend( old_regions.into_iter().map( |region| region.start..region.end ) );

        assert_eq!( new_region_count as i32 - old_region_count as i32, reloaded.regions_mapped.len() as i32 - reloaded.regions_unmapped.len() as i32 );
        reloaded
    }

    fn unwind( &mut self, dwarf_regs: &mut DwarfRegs, stack: &BufferReader, output: &mut Vec< UserFrame > ) {
        output.clear();

        let stack_address = match A::get_stack_pointer( dwarf_regs ) {
            Some( address ) => address,
            None => return
        };

        let memory = Memory {
            regions: &self.regions,
            stack,
            stack_address
        };

        self.ctx.set_panic_on_partial_backtrace( self.panic_on_partial_backtrace );

        let mut ctx = self.ctx.start( &memory, |regs| {
            regs.from_dwarf_regs( dwarf_regs );
        });

        loop {
            let frame = UserFrame {
                address: ctx.current_address(),
                initial_address: ctx.current_initial_address()
            };
            output.push( frame );
            if !ctx.unwind( &memory ) {
                break;
            }
        }
    }

    fn decode_symbol_while< 'a >( &'a self, address: u64, callback: &mut FnMut( &mut Frame< 'a > ) -> bool ) {
        if let Some( region ) = self.regions.get_value( address ) {
            region.binary.decode_symbol_while( address, callback );
        } else {
            let mut frame = Frame {
                absolute_address: address,
                relative_address: address,
                name: None,
                demangled_name: None,
                file: None,
                line: None,
                column: None,
                is_inline: false
            };

            callback( &mut frame );
        }
    }

    fn decode_symbol_once( &self, address: u64 ) -> Frame {
        if let Some( region ) = self.regions.get_value( address ) {
            region.binary.decode_symbol_once( address )
        } else {
            Frame {
                absolute_address: address,
                relative_address: address,
                name: None,
                demangled_name: None,
                file: None,
                line: None,
                column: None,
                is_inline: false
            }
        }
    }

    fn set_panic_on_partial_backtrace( &mut self, value: bool ) {
        self.panic_on_partial_backtrace = value;
    }
}

impl< A: Architecture > AddressSpace< A > {
    pub fn new() -> Self {
        AddressSpace {
            ctx: UnwindContext::< A >::new(),
            binary_map: HashMap::new(),
            regions: RangeMap::new(),
            panic_on_partial_backtrace: false
        }
    }
}

#[test]
fn test_reload() {
    use std::env;
    use std::fs::File;
    use std::io::Read;
    use arch;

    let _ = ::env_logger::try_init();

    fn region( start: u64, inode: u64, name: &str ) -> Region {
        Region {
            start: start,
            end: start + 4096,
            is_read: true,
            is_write: false,
            is_executable: true,
            is_shared: false,
            file_offset: 0,
            major: 0,
            minor: 0,
            inode,
            name: name.to_owned()
        }
    }

    let path = env::current_exe().unwrap();
    let mut raw_data = Vec::new();
    {
        let mut fp = File::open( path ).unwrap();
        fp.read_to_end( &mut raw_data ).unwrap();
    }

    let mut callback = |region: &Region, handle: &mut LoadHandle| {
        handle.should_load_frame_descriptions( false );
        handle.should_load_symbols( false );

        match region.name.as_str() {
            "file_1" | "file_2" | "file_3" => {
                let mut data = BinaryData::load_from_owned_bytes( &region.name, raw_data.clone() ).unwrap();
                let inode = region.name.as_bytes().last().unwrap() - b'1';
                data.set_inode( Inode { inode: inode as _, dev_major: 0, dev_minor: 0 } );
                handle.set_binary( data.into() );
            },
            _ => {}
        }
    };

    let mut address_space = AddressSpace::< arch::native::Arch >::new();

    let mut regions = vec![
        region( 0x1000, 1, "file_1" ),
        region( 0x2000, 2, "file_2" )
    ];

    let res = address_space.reload( regions.clone(), &mut callback );
    assert_eq!( res.binaries_unmapped.len(), 0 );
    assert_eq!( res.binaries_mapped.len(), 2 );
    assert_eq!( res.regions_unmapped.len(), 0 );
    assert_eq!( res.regions_mapped.len(), 2 );

    let res = address_space.reload( regions.clone(), &mut callback );
    assert_eq!( res.binaries_unmapped.len(), 0 );
    assert_eq!( res.binaries_mapped.len(), 0 );
    assert_eq!( res.regions_unmapped.len(), 0 );
    assert_eq!( res.regions_mapped.len(), 0 );

    regions.push( region( 0x3000, 3, "file_3" ) );

    let res = address_space.reload( regions.clone(), &mut callback );
    assert_eq!( res.binaries_unmapped.len(), 0 );
    assert_eq!( res.binaries_mapped.len(), 1 );
    assert_eq!( res.regions_unmapped.len(), 0 );
    assert_eq!( res.regions_mapped.len(), 1 );

    regions.push( region( 0x4000, 3, "file_3" ) );

    let res = address_space.reload( regions.clone(), &mut callback );
    assert_eq!( res.binaries_unmapped.len(), 0 );
    assert_eq!( res.binaries_mapped.len(), 0 );
    assert_eq!( res.regions_unmapped.len(), 0 );
    assert_eq!( res.regions_mapped.len(), 1 );

    regions.pop();
    regions.pop();

    let res = address_space.reload( regions.clone(), &mut callback );
    assert_eq!( res.binaries_unmapped.len(), 1 );
    assert_eq!( res.binaries_mapped.len(), 0 );
    assert_eq!( res.regions_unmapped.len(), 2 );
    assert_eq!( res.regions_mapped.len(), 0 );
}
