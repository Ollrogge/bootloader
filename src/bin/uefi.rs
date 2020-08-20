#![no_std]
#![no_main]
#![feature(abi_efiapi)]
#![feature(asm)]
#![feature(unsafe_block_in_unsafe_fn)]
#![deny(unsafe_op_in_unsafe_fn)]

// Defines the constants `KERNEL_BYTES` (array of `u8`) and `KERNEL_SIZE` (`usize`).
include!(concat!(env!("OUT_DIR"), "/kernel_info.rs"));

static KERNEL: PageAligned<[u8; KERNEL_SIZE]> = PageAligned(KERNEL_BYTES);

#[repr(align(4096))]
struct PageAligned<T>(T);

extern crate rlibc;

use core::{mem, slice};
use uefi::{
    prelude::{entry, Boot, Handle, ResultExt, Status, SystemTable},
    proto::console::gop::{GraphicsOutput, PixelFormat},
    table::boot::{MemoryDescriptor, MemoryType},
};
use x86_64::{
    registers,
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame,
        Size4KiB,
    },
    PhysAddr, VirtAddr,
};

const PAGE_SIZE: u64 = 4096;

#[entry]
fn efi_main(image: Handle, st: SystemTable<Boot>) -> Status {
    let (framebuffer_addr, framebuffer_size) = init_logger(&st);
    log::info!("Hello World from UEFI bootloader!");
    log::info!("Using framebuffer at {:#x}", framebuffer_addr);

    let mmap_storage = {
        let max_mmap_size =
            st.boot_services().memory_map_size() + 8 * mem::size_of::<MemoryDescriptor>();
        let ptr = st
            .boot_services()
            .allocate_pool(MemoryType::LOADER_DATA, max_mmap_size)?
            .log();
        unsafe { slice::from_raw_parts_mut(ptr, max_mmap_size) }
    };

    log::trace!("exiting boot services");
    let (_st, memory_map) = st
        .exit_boot_services(image, mmap_storage)
        .expect_success("Failed to exit boot services");

    let mut frame_allocator = UefiFrameAllocator::new(memory_map);

    let (mut page_table, level_4_frame) = {
        // UEFI identity maps all memory, so physical memory offset is 0
        let phys_offset = VirtAddr::new(0);
        // get an unused frame for new level 4 page table
        let frame: PhysFrame = frame_allocator.allocate_frame().expect("no unused frames");
        log::info!("New page table at: {:#?}", &frame);
        // get the corresponding virtual address
        let addr = phys_offset + frame.start_address().as_u64();
        // initialize a new page table
        let ptr = addr.as_mut_ptr();
        unsafe { *ptr = PageTable::new() };
        let level_4_table = unsafe { &mut *ptr };
        (
            unsafe { OffsetPageTable::new(level_4_table, phys_offset) },
            frame,
        )
    };
    log::info!("New page table at: {:?}", level_4_frame);

    let entry_point = bootloader_lib::load_kernel(&KERNEL.0, &mut page_table, &mut frame_allocator);
    log::info!("Entry point at: {:#x}", entry_point.as_u64());

    // create a stack
    let stack_start: Page = Page::containing_address(VirtAddr::new(0xfff00000000));
    let stack_end = stack_start + 20;
    for page in Page::range(stack_start, stack_end) {
        let frame = frame_allocator
            .allocate_frame()
            .expect("frame allocation failed when mapping a kernel stack");
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe { page_table.map_to(page, frame, flags, &mut frame_allocator) }
            .unwrap()
            .flush();
    }

    // identity-map framebuffer
    let framebuffer_start_frame: PhysFrame = PhysFrame::containing_address(framebuffer_addr);
    let framebuffer_end_frame =
        PhysFrame::containing_address(framebuffer_addr + framebuffer_size - 1u64);
    for frame in PhysFrame::range_inclusive(framebuffer_start_frame, framebuffer_end_frame) {
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe { page_table.identity_map(frame, flags, &mut frame_allocator) }
            .unwrap()
            .flush();
    }

    // reserve two unused frames for context switch and one for the boot info
    let two_frames = TwoFrames::new(&mut frame_allocator);
    let boot_info_frame = frame_allocator.allocate_frame().unwrap();
    // identity-map the boot info frame in new page tables
    unsafe {
        page_table.identity_map(
            boot_info_frame,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
            &mut frame_allocator,
        )
    }
    .unwrap()
    .flush();

    // create memory map
    let memory_map = {
        use bootloader::memory_map::{MemoryMap, MemoryMapBuilder};

        let frame = frame_allocator
            .allocate_frame()
            .expect("frame allocation for memory map failed");
        let memory_map_ptr: *mut MemoryMap =
            VirtAddr::new(frame.start_address().as_u64()).as_mut_ptr();
        let memory_map = unsafe {
            memory_map_ptr.write(MemoryMap::new());
            &mut *memory_map_ptr
        };

        // identity-map the frame in new page tables
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe { page_table.identity_map(frame, flags, &mut frame_allocator) }
            .unwrap()
            .flush();

        let mut builder = MemoryMapBuilder::new(memory_map);
        frame_allocator.construct_memory_map(&mut builder);
        builder.finalize()
    };

    // create boot info
    let boot_info = {
        use bootloader::boot_info_uefi::{BootInfo, FrameBufferInfo};

        let boot_info = BootInfo {
            memory_map,
            framebuffer: FrameBufferInfo {
                start_addr: framebuffer_addr.as_u64(),
                len: framebuffer_size,
            },
        };
        let ptr: *mut BootInfo =
            VirtAddr::new(boot_info_frame.start_address().as_u64()).as_mut_ptr();
        unsafe {
            ptr.write(boot_info);
            &mut *ptr
        }
    };

    let addresses = Addresses {
        page_table: level_4_frame,
        stack_top: stack_end.start_address(),
        entry_point,
        boot_info,
    };
    unsafe {
        context_switch(addresses, page_table, two_frames);
    }
}

unsafe fn context_switch(
    addresses: Addresses,
    mut page_table: OffsetPageTable,
    mut frame_allocator: impl FrameAllocator<Size4KiB>,
) -> ! {
    // identity-map current and next frame, so that we don't get an immediate pagefault
    // after switching the active page table
    let current_addr = PhysAddr::new(registers::read_rip());
    let current_frame: PhysFrame = PhysFrame::containing_address(current_addr);
    for frame in PhysFrame::range_inclusive(current_frame, current_frame + 1) {
        unsafe { page_table.identity_map(frame, PageTableFlags::PRESENT, &mut frame_allocator) }
            .unwrap()
            .flush();
    }

    // we don't need the page table anymore
    mem::drop(page_table);

    // do the context switch
    unsafe {
        asm!(
            "mov cr3, {}; mov rsp, {}; push 0; jmp {}",
            in(reg) addresses.page_table.start_address().as_u64(),
            in(reg) addresses.stack_top.as_u64(),
            in(reg) addresses.entry_point.as_u64(),
            in("rdi") addresses.boot_info as *const _ as usize,
        );
    }
    unreachable!();
}

struct Addresses {
    page_table: PhysFrame,
    stack_top: VirtAddr,
    entry_point: VirtAddr,
    boot_info: &'static mut bootloader::boot_info_uefi::BootInfo,
}

fn init_logger(st: &SystemTable<Boot>) -> (PhysAddr, usize) {
    let gop = st
        .boot_services()
        .locate_protocol::<GraphicsOutput>()
        .expect_success("failed to locate gop");
    let gop = unsafe { &mut *gop.get() };

    let mode_info = gop.current_mode_info();
    let mut framebuffer = gop.frame_buffer();
    let slice = unsafe { slice::from_raw_parts_mut(framebuffer.as_mut_ptr(), framebuffer.size()) };
    let info = bootloader_lib::FrameBufferInfo {
        horizontal_resolution: mode_info.resolution().0,
        vertical_resolution: mode_info.resolution().1,
        pixel_format: match mode_info.pixel_format() {
            PixelFormat::RGB => bootloader_lib::PixelFormat::BGR,
            PixelFormat::BGR => bootloader_lib::PixelFormat::BGR,
            PixelFormat::Bitmask | PixelFormat::BltOnly => {
                panic!("Bitmask and BltOnly framebuffers are not supported")
            }
        },
        stride: mode_info.stride(),
    };

    bootloader_lib::init_logger(slice, info);

    (
        PhysAddr::new(framebuffer.as_mut_ptr() as u64),
        framebuffer.size(),
    )
}

struct UefiFrameAllocator<'a, I> {
    original: I,
    memory_map: I,
    current_descriptor: Option<&'a MemoryDescriptor>,
    next_frame: PhysFrame,
}

impl<'a, I> UefiFrameAllocator<'a, I>
where
    I: Iterator<Item = &'a MemoryDescriptor> + Clone,
{
    fn new(memory_map: I) -> Self {
        Self {
            original: memory_map.clone(),
            memory_map,
            current_descriptor: None,
            next_frame: PhysFrame::containing_address(PhysAddr::new(0)),
        }
    }

    fn allocate_frame_from_descriptor(
        &mut self,
        descriptor: &MemoryDescriptor,
    ) -> Option<PhysFrame> {
        let start_addr = PhysAddr::new(descriptor.phys_start);
        let start_frame: PhysFrame = PhysFrame::containing_address(start_addr.align_up(PAGE_SIZE));
        let end_frame = start_frame + descriptor.page_count;
        if self.next_frame < end_frame {
            let ret = self.next_frame;
            self.next_frame += 1;
            Some(ret)
        } else {
            None
        }
    }

    fn construct_memory_map(self, builder: &mut bootloader::memory_map::MemoryMapBuilder) {
        use bootloader::memory_map::{MemoryMap, MemoryRegion, MemoryRegionKind};

        for mut descriptor in self.original.copied() {
            let end = descriptor.phys_start + PAGE_SIZE * descriptor.page_count;
            let next_free = self.next_frame.start_address().as_u64();
            let kind = match descriptor.ty {
                MemoryType::CONVENTIONAL if end <= next_free => MemoryRegionKind::Bootloader,
                MemoryType::CONVENTIONAL if descriptor.phys_start >= next_free => {
                    MemoryRegionKind::Usable
                }
                MemoryType::CONVENTIONAL => {
                    // part of the region is used -> add is separately
                    let used_region = MemoryRegion {
                        start: descriptor.phys_start,
                        end: next_free,
                        kind: MemoryRegionKind::Bootloader,
                    };
                    builder.add_region(used_region);

                    // add unused part normally
                    descriptor.phys_start = next_free;
                    MemoryRegionKind::Usable
                }
                MemoryType::RESERVED => MemoryRegionKind::Reserved,
                other => continue,
            };
            let region = MemoryRegion {
                start: descriptor.phys_start,
                end,
                kind,
            };
            builder.add_region(region);
        }
    }
}

unsafe impl<'a, I> FrameAllocator<Size4KiB> for UefiFrameAllocator<'a, I>
where
    I: Iterator<Item = &'a MemoryDescriptor> + Clone,
{
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        if let Some(current_descriptor) = self.current_descriptor {
            match self.allocate_frame_from_descriptor(current_descriptor) {
                Some(frame) => return Some(frame),
                None => {
                    self.current_descriptor = None;
                }
            }
        }

        // find next suitable descriptor
        while let Some(descriptor) = self.memory_map.next() {
            if descriptor.ty != MemoryType::CONVENTIONAL {
                continue;
            }
            if let Some(frame) = self.allocate_frame_from_descriptor(descriptor) {
                self.current_descriptor = Some(descriptor);
                return Some(frame);
            }
        }

        None
    }
}

pub struct TwoFrames {
    frames: [Option<PhysFrame>; 2],
}

impl TwoFrames {
    pub fn new(frame_allocator: &mut impl FrameAllocator<Size4KiB>) -> Self {
        TwoFrames {
            frames: [
                Some(frame_allocator.allocate_frame().unwrap()),
                Some(frame_allocator.allocate_frame().unwrap()),
            ],
        }
    }
}

unsafe impl FrameAllocator<Size4KiB> for TwoFrames {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        self.frames.iter_mut().find_map(|f| f.take())
    }
}
