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
    use cpal::platform::{ScreenCaptureKitDevice, ScreenCaptureKitDevices};

    println!("=== ScreenCaptureKit Performance Timing ===\n");

    // 1. Get ScreenCaptureKit Host
    let t0 = Instant::now();
    let host = cpal::host_from_id(HostId::ScreenCaptureKit)
        .expect("ScreenCaptureKit host not found. Are you on macOS 12.3+?");
    println!("[TIMING] Host creation: {:?}", t0.elapsed());

    println!("Host: {:?}", host.id());

    // 2. enumerate devices
    let t1 = Instant::now();
    let devices = host.devices()?;
    println!("[TIMING] devices() call: {:?}", t1.elapsed());

    let t2 = Instant::now();
    println!("Available devices:");
    for device in devices {
        println!("  - {}", device.description()?.name());
    }
    println!("[TIMING] Iterating devices: {:?}", t2.elapsed());

    // 3. Get available applications and find QQ Music
    println!("\n=== Finding QQ Music to exclude ===");
    let t_apps = Instant::now();
    let apps = ScreenCaptureKitDevices::available_applications()?;
    println!("[TIMING] available_applications(): {:?}", t_apps.elapsed());

    println!("Running applications ({} total):", apps.len());
    let mut excluded_apps = Vec::new();

    for app in &apps {
        let bundle_id = unsafe { app.bundleIdentifier().to_string() };
        let app_name = unsafe { app.applicationName().to_string() };

        // Print all apps for debugging
        if app_name.contains("QQ音乐") {
            println!("  🎵 [FOUND] {} ({})", app_name, bundle_id);
            excluded_apps.push(app.clone());
        }
    }

    if excluded_apps.is_empty() {
        println!("  ⚠️ QQ Music not found! Please start QQ Music and try again.");
        println!("  Looking for apps with 'QQ' or 'tencent' in name/bundle ID...");

        // Show some apps for reference
        for app in apps.iter().take(10) {
            let bundle_id = unsafe { app.bundleIdentifier().to_string() };
            let app_name = unsafe { app.applicationName().to_string() };
            println!("    - {} ({})", app_name, bundle_id);
        }
    } else {
        println!("\n  ✅ Found {} app(s) to exclude", excluded_apps.len());
    }

    // 4. Get default input device and set excluded apps
    let t3 = Instant::now();
    let device = host
        .default_input_device()
        .expect("No input device available");
    println!("[TIMING] default_input_device(): {:?}", t3.elapsed());

    // Convert to ScreenCaptureKitDevice to access set_excluded_apps
    let mut sck_device: ScreenCaptureKitDevice = match device.into_inner() {
        cpal::platform::DeviceInner::ScreenCaptureKit(d) => d,
        _ => panic!("Expected ScreenCaptureKit device"),
    };

    // Set excluded apps (QQ Music)
    if !excluded_apps.is_empty() {
        sck_device.set_excluded_apps(&excluded_apps);
        println!(
            "\n🔇 Excluding {} app(s) from audio capture",
            excluded_apps.len()
        );
    }

    println!("Selected device: {}", sck_device.description()?.name());

    // 5. Configure stream
    let t4 = Instant::now();
    let supported_config = sck_device.default_input_config().unwrap();
    println!("[TIMING] default_input_config(): {:?}", t4.elapsed());

    println!("Default config: {:?}", supported_config);
    let sample_format = supported_config.sample_format();

    let config: cpal::StreamConfig = supported_config.into();

    // 6. Build input stream
    let t5 = Instant::now();
    println!("Building input stream...");
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

    println!("Stream built. Playing...");
    println!("[TIMING] build_input_stream_raw(): {:?}", t5.elapsed());

    let t6 = Instant::now();
    stream.play()?;
    println!("[TIMING] stream.play(): {:?}", t6.elapsed());

    println!("\n=== Total initialization time: {:?} ===\n", t0.elapsed());

    println!("Recording for 10 seconds...");
    println!("🔊 If QQ Music is playing, you should see NO audio output (RMS values)");
    println!("🔊 If other apps are playing audio, you WILL see their audio\n");
    std::thread::sleep(std::time::Duration::from_secs(10));

    println!("Stopping...");
    stream.pause()?; // Optional, drop handles it

    Ok(())
}
