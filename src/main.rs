use chrono;
use core_graphics::display::{CGDirectDisplayID, CGDisplay};
use eframe::egui;
use egui::ViewportBuilder;
use gstreamer::prelude::Cast;
use gstreamer::{self as gst, DeviceMonitor};
use gstreamer::{prelude::*, DeviceMonitorFilterId};
use gstreamer_app;
use std::sync::mpsc;

struct ScreenCapApp {
    texture: Option<egui::TextureHandle>,
    receiver: mpsc::Receiver<Vec<u8>>,
    sender: mpsc::Sender<Vec<u8>>,
    width: i32,
    height: i32,
    is_recording: bool,
    pipeline: gst::Pipeline,
    recording_bin: Option<gst::Element>,
    current_frame: Option<Vec<u8>>,
    current_device_id: String,
    gstreamer_devices: Vec<MediaDeviceInfo>,
    show_settings: bool,
    image_size: egui::Vec2,
}

#[derive(Debug)]
struct MediaDeviceInfo {
    device_id: String,
    kind: MediaDeviceKind,
    label: String,
}

#[derive(Debug, PartialEq)]
enum MediaDeviceKind {
    AudioInput,
    AudioOutput,
    VideoInput,
}

fn get_devices() -> Result<Vec<MediaDeviceInfo>, ()> {
    const AUDIO_SOURCE: &str = "Audio/Source";
    const AUDIO_SINK: &str = "Audio/Sink";
    const VIDEO_SOURCE: &str = "Video/Source";
    const VIDEO_RAW: &str = "video/x-raw";
    const AUDIO_RAW: &str = "audio/x-raw";

    let device_monitor = DeviceMonitor::new();

    // Ensure monitor is stopped before starting
    // if device_monitor.is_started() {
    //     device_monitor.stop();
    // }

    device_monitor.start().map_err(|_| ())?;

    device_monitor.add_filter(Some(AUDIO_SOURCE), None);
    device_monitor.add_filter(Some(AUDIO_SINK), None);
    device_monitor.add_filter(Some(AUDIO_RAW), None);
    device_monitor.add_filter(Some(VIDEO_SOURCE), None);
    device_monitor.add_filter(Some(VIDEO_RAW), None);

    let devices = device_monitor
        .devices()
        .iter()
        .filter_map(|device| {
            let display_name = device.display_name().as_str().to_owned();
            println!("Device: {:?}", device.display_name());
            println!("Properties: {:?}", device.properties());
            println!("Device Class: {:?}", device.device_class());
            println!("---");
            Some(MediaDeviceInfo {
                device_id: display_name.clone(),
                kind: match device.device_class().as_str() {
                    AUDIO_SOURCE => MediaDeviceKind::AudioInput,
                    AUDIO_SINK => MediaDeviceKind::AudioOutput,
                    VIDEO_SOURCE => MediaDeviceKind::VideoInput,
                    _ => return None,
                },
                label: display_name,
            })
        })
        .collect();

    // Clean up monitor
    device_monitor.stop();

    Ok(devices)
}

impl ScreenCapApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (receiver, sender, width, height, pipeline, gstreamer_devices) =
            setup_gstreamer(0).unwrap();

        Self {
            gstreamer_devices,
            texture: None,
            receiver,
            sender,
            width,
            height,
            is_recording: false,
            pipeline,
            recording_bin: None,
            current_frame: None,
            current_device_id: String::new(),
            show_settings: true,
            image_size: egui::Vec2::new(width as f32, height as f32),
        }
    }

    fn create_source_pipeline(&self, device_id: String) -> String {
        match device_id.as_str() {
            "Camera" => {
                "avfvideosrc device-index=0 ! video/x-raw,width=1280,height=720,framerate=30/1"
                    .to_string()
            }
            _ => {
                format!(
                    "avfvideosrc capture-screen=true capture-screen-cursor=true device-index={} ! video/x-raw,framerate=30/1",
                    device_id
                )
            }
        }
    }

    pub fn get_current_frame(&self) -> Option<&[u8]> {
        self.current_frame.as_deref()
    }

    pub fn get_dimensions(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    pub fn get_pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || x >= self.width || y < 0 || y >= self.height {
            return None;
        }

        self.current_frame.as_ref().map(|frame| {
            let idx = ((y * self.width + x) * 4) as usize;
            [frame[idx], frame[idx + 1], frame[idx + 2], frame[idx + 3]]
        })
    }

    pub fn get_region_brightness(&self, x: i32, y: i32, width: i32, height: i32) -> Option<f32> {
        let mut sum = 0.0;
        let mut count = 0;

        for cy in y..y + height {
            for cx in x..x + width {
                if let Some([r, g, b, _a]) = self.get_pixel(cx, cy) {
                    sum += (r as f32 + g as f32 + b as f32) / (3.0 * 255.0);
                    count += 1;
                }
            }
        }

        if count > 0 {
            Some(sum / count as f32)
        } else {
            None
        }
    }

    fn get_source_dimensions(device_id: u32) -> (i32, i32) {
        if device_id == 0 {
            // Camera default HD resolution
            (1280, 720)
        } else {
            // Get display resolution
            let displays = CGDisplay::active_displays().unwrap_or_default();

            if device_id as usize <= displays.len() {
                let display = CGDisplay::new(displays[(device_id - 1) as usize]);
                (display.pixels_wide() as i32, display.pixels_high() as i32)
            } else {
                // Fallback resolution
                (1920, 1080)
            }
        }
    }
}

impl eframe::App for ScreenCapApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        println!("On exit");
        // Stop recording if active
        if self.is_recording {
            if let Some(tee) = self.pipeline.by_name("t") {
                tee.set_property("allow-not-linked", false);
            }
        }

        // Stop the pipeline
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            eprintln!("Error stopping pipeline: {:?}", e);
        }

        std::process::exit(0);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Ok(buffer) = self.receiver.try_recv() {
            self.current_frame = Some(buffer.clone());

            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [self.width as usize, self.height as usize],
                &buffer,
            );

            self.texture = Some(ctx.load_texture(
                "screen-capture",
                color_image,
                egui::TextureOptions::default(),
            ));
        }

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Screen Capture");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙").clicked() {
                        // Toggle settings visibility
                        self.show_settings = !self.show_settings;
                    }
                    let record_button = if self.is_recording {
                        ui.button("⏹").on_hover_text("Stop Recording")
                    } else {
                        ui.button("⏺").on_hover_text("Start Recording")
                    };

                    if record_button.clicked() {
                        self.is_recording = !self.is_recording;
                        if self.is_recording {
                            let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
                            let filename = format!("recording_{}.mp4", timestamp);
                            if let Some(filesink) = self.pipeline.by_name("filesink") {
                                filesink.set_property("location", &filename);
                            }
                            if let Some(tee) = self.pipeline.by_name("t") {
                                tee.set_property("allow-not-linked", true);
                            }
                            println!("Started recording to {}", filename);
                        } else {
                            if let Some(tee) = self.pipeline.by_name("t") {
                                tee.set_property("allow-not-linked", false);
                            }
                            println!("Stopped recording");
                        }
                    }
                });
            });
        });

        egui::SidePanel::right("settings_panel").show_animated(ctx, self.show_settings, |ui| {
            ui.heading("Settings");
            ui.separator();

            ui.label("Source Selection");
            ui.horizontal_wrapped(|ui| {
                for device_id in vec![0, 1, 2, 3] {
                    let is_active = self.current_device_id == device_id.to_string();
                    let mut button =
                        egui::Button::new(format!("Source {}", device_id)).selected(is_active);

                    if ui.add(button).clicked() && !is_active {
                        // Stop the current pipeline first
                        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
                            eprintln!("Error stopping pipeline: {:?}", e);
                        }

                        // Start the new pipeline with error handling
                        match setup_gstreamer(device_id) {
                            Ok((receiver, sender, width, height, pipeline, gstreamer_devices)) => {
                                self.receiver = receiver;
                                self.sender = sender;
                                self.width = width;
                                self.height = height;
                                self.pipeline = pipeline;
                                self.gstreamer_devices = gstreamer_devices;
                            }
                            Err(e) => {
                                println!("Failed to start pipeline: {:?}", e);
                            }
                        }
                    }
                }
            });

            ui.separator();
            ui.label("Statistics");
            ui.label(format!("State: {:?}", self.pipeline.current_state()));
            if let Some(pos) = self.pipeline.query_position::<gst::ClockTime>() {
                ui.label(format!("Position: {:.1}s", pos.seconds() as f32 / 1.0));
            }
            if let Some(brightness) = self.get_region_brightness(0, 0, 100, 100) {
                ui.label(format!("Brightness: {:.1}%", brightness * 100.0));
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(texture) = &self.texture {
                // Calculate image size to maintain aspect ratio and fit the panel
                let available_size = ui.available_size();
                let aspect_ratio = self.width as f32 / self.height as f32;
                let mut size = available_size;

                if available_size.x / available_size.y > aspect_ratio {
                    size.x = available_size.y * aspect_ratio;
                } else {
                    size.y = available_size.x / aspect_ratio;
                }

                ui.centered_and_justified(|ui| {
                    // Use a custom widget for resizable image
                    let response = ui.add(
                        egui::Image::new(texture)
                            .fit_to_exact_size(size)
                            .sense(egui::Sense::drag()),
                    );

                    // Handle resizing
                    if response.dragged() {
                        let delta = response.drag_delta();
                        // Maintain aspect ratio while resizing
                        if delta.x.abs() > delta.y.abs() {
                            size.x = (size.x + delta.x).max(100.0);
                            size.y = size.x / aspect_ratio;
                        } else {
                            size.y = (size.y + delta.y).max(100.0);
                            size.x = size.y * aspect_ratio;
                        }
                    }
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.heading("No video input");
                });
            }
        });

        ctx.request_repaint();
    }
}

fn setup_gstreamer(
    device_id: u32,
) -> Result<
    (
        mpsc::Receiver<Vec<u8>>,
        mpsc::Sender<Vec<u8>>,
        i32,
        i32,
        gst::Pipeline,
        Vec<MediaDeviceInfo>,
    ),
    gst::FlowError,
> {
    gst::init().unwrap();

    let monitor = DeviceMonitor::new();

    // Get the correct dimensions for the source
    let (width, height) = ScreenCapApp::get_source_dimensions(device_id);

    let source_element = match std::env::consts::OS {
        "macos" => "avfvideosrc capture-screen=true",
        "windows" => "d3d11screencapturesrc",
        "linux" => "ximagesrc",
        _ => panic!("Unsupported operating system"),
    };

    let source_element = format!("{} device-index={}", source_element, device_id);
    let pipeline_str = format!(
        "{} ! videoscale ! video/x-raw,width={},height={},framerate=30/1 ! tee name=t ! \
         queue ! videoconvert ! video/x-raw,format=RGBA ! appsink name=sink",
        source_element, width, height
    );

    let (sender, receiver) = mpsc::channel();
    let cloned_sender = sender.clone();

    // Create and set up pipeline
    let pipeline = gst::parse::launch(&pipeline_str)
        .map_err(|_| gst::FlowError::Error)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| gst::FlowError::Error)?;

    let appsink = pipeline
        .by_name("sink")
        .ok_or(gst::FlowError::Error)?
        .downcast::<gstreamer_app::AppSink>()
        .map_err(|_| gst::FlowError::Error)?;

    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                sender.send(map.to_vec()).map_err(|_| {
                    eprintln!("Failed to send frame through channel");
                    gst::FlowError::Error
                })?;
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    if let Some(tee) = pipeline.by_name("t") {
        tee.set_property("allow-not-linked", false);
    }

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|_| gst::FlowError::Error)?;

    // Clean up at the end
    monitor.stop();

    Ok((
        receiver,
        cloned_sender,
        width,
        height,
        pipeline,
        get_devices().unwrap_or_else(|_| vec![]),
    ))
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default().with_inner_size([800.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Screen Capture",
        options,
        Box::new(|cc| Ok(Box::new(ScreenCapApp::new(cc)))),
    )
}
