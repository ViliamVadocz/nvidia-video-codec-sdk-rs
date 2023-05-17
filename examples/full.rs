use std::{
    ffi::{c_int, c_void},
    fs::{File, OpenOptions},
    io::Write,
    os::fd::AsRawFd,
    ptr,
    sync::Arc,
};

use cudarc::driver::{
    sys::{
        cuExternalMemoryGetMappedBuffer, cuImportExternalMemory,
        CUDA_EXTERNAL_MEMORY_HANDLE_DESC_st__bindgen_ty_1, CUdeviceptr, CUexternalMemory,
        CUexternalMemoryHandleType, CUresult, CUDA_EXTERNAL_MEMORY_BUFFER_DESC,
        CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
    },
    CudaDevice,
};
use vulkano::{
    device::{
        physical::PhysicalDeviceType, Device, DeviceCreateInfo, DeviceExtensions, QueueCreateInfo,
    },
    instance::{Instance, InstanceCreateInfo},
    memory::{
        DeviceMemory, ExternalMemoryHandleType, ExternalMemoryHandleTypes, MappedDeviceMemory,
        MemoryAllocateInfo, MemoryPropertyFlags,
    },
    VulkanLibrary,
};

#[allow(deprecated)]
use nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_PRESET_LOW_LATENCY_HP_GUID;
use nvidia_video_codec_sdk::{
    safe::{api::ENCODE_API, buffer::MappedResource, encoder::Encoder},
    sys::nvEncodeAPI::{
        NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB, NV_ENC_CODEC_H264_GUID,
        NV_ENC_H264_PROFILE_HIGH_GUID, NV_ENC_INITIALIZE_PARAMS, NV_ENC_INPUT_RESOURCE_TYPE,
        NV_ENC_PIC_PARAMS, NV_ENC_PIC_STRUCT, NV_ENC_REGISTER_RESOURCE, NV_ENC_TUNING_INFO,
        _NV_ENC_BUFFER_FORMAT,
    },
};

/// Returns the color `(r, g, b, alpha)` of a pixel on the screen relative to its position on a screen:
///
/// # Params
/// `width`, `height`: Width and height of the screen.
///
/// `x`, `y`, `z`: Coördinates of the pixel on the screen.
///
/// # Function
///
/// Top right will be red,
/// bottom left will be green,
/// all colours will shift towards having more blue as `time` increases.
fn get_color(width: u32, height: u32, x: u32, y: u32, time: f32) -> (u8, u8, u8, u8) {
    let alpha = 255;
    let red = (255 * x / width) as u8;
    let green = (255 * y / height) as u8;
    let blue = (255. * time) as u8;
    (blue, green, red, alpha)
}

/// Generates a
fn generate_test_input(buf: &mut [u8], width: u32, height: u32, i: u32, i_max: u32) {
    assert_eq!(buf.len(), (width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let pixel = width * y + x;
            let index = (pixel * 4) as usize;
            let color = get_color(width, height, x, y, i as f32 / i_max as f32);
            buf[index] = color.0;
            buf[index + 1] = color.1;
            buf[index + 2] = color.2;
            buf[index + 3] = color.3;
        }
    }
}

/// Creates a bitstream for a 1920x1080 video.
fn main() {
    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FRAMES: u32 = 128;

    // Initialize Vulkan.
    let vulkan_library = VulkanLibrary::new().unwrap();
    let instance = Instance::new(
        vulkan_library,
        InstanceCreateInfo::application_from_cargo_toml(),
    )
    .expect("Vulkan should be installed correctly");

    let (memory_type_index, physical_device) = instance
        .enumerate_physical_devices()
        .unwrap()
        .filter_map(|pd| {
            matches!(pd.properties().device_type, PhysicalDeviceType::DiscreteGpu)
                .then_some(())
                .and_then(|()| {
                    pd.memory_properties()
                        .memory_types
                        .iter()
                        .position(|mt| {
                            mt.property_flags
                                .contains(MemoryPropertyFlags::HOST_VISIBLE)
                        })
                        .map(|index| (index as u32, pd))
                })
        })
        .next()
        .expect("There should be at least one GPU, which supports a memory type that is `HOST_VISISBLE`");

    // Create a Vulkan device.
    let (vulkan_device, _queues) = Device::new(
        physical_device,
        DeviceCreateInfo {
            queue_create_infos: vec![QueueCreateInfo::default()],
            enabled_extensions: DeviceExtensions {
                khr_external_memory_fd: true,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .expect(
        "Vulkan should be installed correctly and `Device` should support `khr_external_memory_fd`",
    );

    let cuda_device = CudaDevice::new(0).unwrap();

    let encoder = ENCODE_API
        .open_encode_session_with_cuda(cuda_device)
        .expect("Cuda should be installed correctly");

    // Get all encode guids supported by the GPU.
    let encode_guids = encoder
        .get_encode_guids()
        .expect("The encoder should be able to get the supported guids");
    let encode_guid = NV_ENC_CODEC_H264_GUID;
    assert!(encode_guids.contains(&encode_guid));

    // Get available preset guids based on encode guid.
    let preset_guids = encoder
        .get_preset_guids(encode_guid)
        .expect("The encoder should have a preset for H.264");
    #[allow(deprecated)]
    let preset_guid = NV_ENC_PRESET_LOW_LATENCY_HP_GUID;
    assert!(preset_guids.contains(&preset_guid));

    // Get available profiles based on encode guid.
    let profile_guids = encoder
        .get_profile_guids(encode_guid)
        .expect("The encoder should have a profile for H.264");
    let profile_guid = NV_ENC_H264_PROFILE_HIGH_GUID;
    assert!(profile_guids.contains(&profile_guid));

    // Get input formats based on the encode guid.
    let input_formats = encoder
        .get_supported_input_formats(encode_guid)
        .expect("The encoder should be able to receive inputs");
    let buffer_format = NV_ENC_BUFFER_FORMAT_ARGB;
    assert!(input_formats.contains(&buffer_format));

    // Get the preset config based on the selected encode guid (H.264) and selected preset (`LOW_LATENCY`).
    let mut preset_config = encoder
        .get_preset_config(
            encode_guid,
            preset_guid,
            NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
        )
        .expect("Encoder should be able to create config based on presets");

    // Initialise a new encoder session based on the `preset_config` we generated before.
    encoder
        .initialize_encoder_session(
            NV_ENC_INITIALIZE_PARAMS::new(encode_guid, WIDTH, HEIGHT)
                .display_aspect_ratio(16, 9)
                .framerate(30, 1)
                .enable_picture_type_decision()
                .encode_config(&mut preset_config.presetCfg),
        )
        .expect("Encoder should be initialised correctly");

    // Calculate the number of buffs we need based on the interval of P frames and(+) the look ahead depth
    let num_bufs = usize::try_from(preset_config.presetCfg.frameIntervalP)
        .expect("frame intervalP should always be positive")
        + usize::try_from(preset_config.presetCfg.rcParams.lookaheadDepth)
            .expect("lookahead depth should always be positive");

    let mut output_buffers: Vec<_> = (0..num_bufs)
        .map(|_| {
            encoder
                .create_output_bitstream()
                .expect("The encoder should be able to create bitstreams")
        })
        .collect();

    // Write result to output file "test.bin".
    let mut out_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open("test.bin")
        .expect("Should be able to acces the file system and create a new file");

    // Create Vulkan buffers
    // 4.1.2. Input buffers allocated externally

    // Generate each of the frames
    let file_descriptors = (1..FRAMES)
        .map(|f| {
            create_buffer(
                buffer_format,
                vulkan_device,
                memory_type_index,
                WIDTH,
                HEIGHT,
                f,
                FRAMES,
            )
        })
        .collect::<Vec<_>>();

    // Generate each of the frames
    for (i, file_descriptor) in file_descriptors.into_iter().enumerate() {
        let output_buffer = &mut output_buffers[i % num_bufs];

        let input_buffer = make_vulkan_buffer(
            &encoder,
            buffer_format,
            memory_type_index,
            WIDTH,
            HEIGHT,
            file_descriptor,
        );

        encoder
            .encode_picture(NV_ENC_PIC_PARAMS::new(
                WIDTH,
                HEIGHT,
                &mut input_buffer,
                output_buffer,
                buffer_format,
                NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            ))
            .expect("Encoder should be able to encode valid pictures");

        // TODO: only read if encode_picture was Ok().
        // It could also ask for more input data!
        let out = output_buffer
            .lock_and_read()
            .expect("Buffer should be fully filled and not locked");
        out_file
            .write_all(out)
            .expect("Should be able to acces the file system and write to a `out_file`");
    }

    // Notifying the End of Input Stream with end of stream picture.
    // Note that output is still generated here.
    let mut input_buffer = make_vulkan_buffer(
        &encoder,
        buffer_format,
        memory_type_index,
        WIDTH,
        HEIGHT,
        FRAMES,
    );

    let output_buffer = &mut output_buffers[0];
    encoder
        .encode_picture(
            NV_ENC_PIC_PARAMS::new(
                WIDTH,
                HEIGHT,
                &mut input_buffer,
                output_buffer,
                buffer_format,
                NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            )
            .end_of_stream(),
        )
        .unwrap();

    let out = output_buffer
        .lock_and_read()
        .expect("Buffer should be fully filled and not locked");
    out_file
        .write_all(out)
        .expect("Should be able to acces the file system and write to a `out_file`");
}

fn create_buffer(
    buffer_format: _NV_ENC_BUFFER_FORMAT,
    vulkan_device: Arc<Device>,
    memory_type_index: u32,
    width: u32,
    height: u32,
    i: u32,
    i_max: u32,
) -> File {
    let size = (width * height * 4) as u64;
    // Allocate memory with Vulkan.
    let memory = DeviceMemory::allocate(
        vulkan_device,
        MemoryAllocateInfo {
            allocation_size: size,
            memory_type_index,
            export_handle_types: ExternalMemoryHandleTypes::OPAQUE_FD,
            ..Default::default()
        },
    )
    .unwrap();

    // Map and write to the memory.
    let mapped_memory = MappedDeviceMemory::new(memory, 0..size).unwrap();
    unsafe {
        let content = mapped_memory.write(0..size).unwrap();
        generate_test_input(content, width, height, i, i_max);
        mapped_memory.flush_range(0..size).unwrap();
    }
    // Export the memory.
    mapped_memory
        .unmap()
        .export_fd(ExternalMemoryHandleType::OpaqueFd)
        .unwrap()
}

fn make_vulkan_buffer(
    encoder: &Encoder,
    buffer_format: _NV_ENC_BUFFER_FORMAT,
    memory_type_index: u32,
    width: u32,
    height: u32,
    file: File,
) -> MappedResource {
    let size = (width * height * 4) as u64;
    // Import file handle with CUDA.
    let mut external_memory: CUexternalMemory = ptr::null_mut();
    let handle_description = CUDA_EXTERNAL_MEMORY_HANDLE_DESC {
        type_: CUexternalMemoryHandleType::CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
        handle: CUDA_EXTERNAL_MEMORY_HANDLE_DESC_st__bindgen_ty_1 {
            fd: file.as_raw_fd() as c_int,
        },
        size,
        ..Default::default()
    };
    assert_eq!(CUresult::CUDA_SUCCESS, unsafe {
        cuImportExternalMemory(&mut external_memory, &handle_description)
    });
    // Get mapped buffer.
    let mut device_ptr: CUdeviceptr = 0;
    let buffer_description = CUDA_EXTERNAL_MEMORY_BUFFER_DESC {
        size,
        ..Default::default()
    };
    assert_eq!(CUresult::CUDA_SUCCESS, unsafe {
        cuExternalMemoryGetMappedBuffer(&mut device_ptr, external_memory, &buffer_description)
    });
    // Register and map it with NVENC.
    let (input_resource, buf_fmt) = encoder
        .register_and_map_input_resource(
            NV_ENC_REGISTER_RESOURCE::new(
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                width,
                height,
                device_ptr as *mut c_void,
                buffer_format,
            )
            .pitch(width * 4),
        )
        .unwrap();
    assert_eq!(buffer_format, buf_fmt);
    input_resource
    // TODO: Should the CUDA mapping, etc. be undone to free the memory?
}
