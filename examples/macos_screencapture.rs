use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::HostId;
use std::time::Instant;

fn main() -> Result<(), anyhow::Error> {
    #[cfg(not(target_os = "macos"))]
    {
        println!("This example is only for macOS.");
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        run_macos_example()
    }
}

#[cfg(target_os = "macos")]
fn run_macos_example() -> Result<(), anyhow::Error> {
    use cpal::platform::ScreenCaptureKitDevice;

    println!("=== ScreenCaptureKit Audio Capture with App Filtering ===\n");

    // 1. Get ScreenCaptureKit Host
    let t0 = Instant::now();
    let host = cpal::host_from_id(HostId::ScreenCaptureKit)
        .expect("ScreenCaptureKit host not found. Are you on macOS 12.3+?");
    println!("[TIMING] Host creation: {:?}", t0.elapsed());

    println!("Host: {:?}", host.id());

    // 2. Get default input device
    let t1 = Instant::now();
    let device = host
        .default_input_device()
        .expect("No input device available");
    println!("[TIMING] default_input_device(): {:?}", t1.elapsed());

    // Convert to ScreenCaptureKitDevice to access set_excluded_app_names
    let mut sck_device: ScreenCaptureKitDevice = match device.into_inner() {
        cpal::platform::DeviceInner::ScreenCaptureKit(d) => d,
        _ => panic!("Expected ScreenCaptureKit device"),
    };

    // 3. Set excluded apps by name (the new simple API!)
    //    第一次调用 build_input_stream 时会自动获取并缓存应用列表
    println!("\n=== Setting up app exclusion ===");
    let excluded_apps = ["QQ音乐"]; // 只排除 QQ音乐
    sck_device.set_excluded_app_names(&excluded_apps);
    println!("🔇 Will exclude apps matching: {:?}", excluded_apps);

    println!("Selected device: {}", sck_device.description()?.name());

    // 4. Configure stream
    let t2 = Instant::now();
    let supported_config = sck_device.default_input_config().unwrap();
    println!("[TIMING] default_input_config(): {:?}", t2.elapsed());

    println!("Default config: {:?}", supported_config);
    let sample_format = supported_config.sample_format();

    let config: cpal::StreamConfig = supported_config.into();

    // 5. Build input stream (app names will be matched against cached apps here)
    let t3 = Instant::now();
    println!("\nBuilding input stream (will fetch app list if not cached)...");
    let stream = sck_device.build_input_stream_raw(
        &config,
        sample_format,
        move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
            // Process data
            if let Some(samples) = data.as_slice::<f32>() {
                let len = samples.len();
                let mut rms = 0.0;
                for &sample in samples {
                    rms += sample * sample;
                }
                rms = (rms / len as f32).sqrt();
                if rms > 0.001 {
                    println!("Received {} samples. RMS: {:.4}", len, rms);
                }
            }
        },
        move |err| {
            eprintln!("Stream error: {:?}", err);
        },
        None, // Timeout
    )?;
    println!("[TIMING] build_input_stream_raw(): {:?}", t3.elapsed());

    println!("Stream built. Playing...");

    let t4 = Instant::now();
    stream.play()?;
    println!("[TIMING] stream.play(): {:?}", t4.elapsed());

    println!("\n=== Total initialization time: {:?} ===\n", t0.elapsed());

    println!("Recording for 10 seconds...");
    println!("🔊 If QQ音乐 is playing, you should see NO audio output");
    println!("🔊 If other apps are playing audio, you WILL see their audio\n");
    std::thread::sleep(std::time::Duration::from_secs(10));

    println!("Stopping...");
    stream.pause()?; // Optional, drop handles it

    Ok(())
}
