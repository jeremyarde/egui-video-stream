use chrono;
use eframe::egui;
use egui::ViewportBuilder;
use gstreamer::glib;
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
    id: u32,
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
    const VIDEO_SOURCE: &str = "Video/Source";
    const VIDEO_RAW: &str = "video/x-raw";
    const VIDEO_OUTPUT: &str = "Video/Output";

    let device_monitor = DeviceMonitor::new();

    // Add video-specific filters BEFORE starting the monitor
    device_monitor.add_filter(Some(VIDEO_SOURCE), None);
    device_monitor.add_filter(Some(VIDEO_RAW), None);
    device_monitor.add_filter(Some(VIDEO_OUTPUT), None);

    let _ = device_monitor.start();

    // Get hardware devices (like cameras)
    let mut devices = device_monitor
        .devices()
        .iter()
        .filter_map(|device| {
            println!("Found device: {:?}", device);

            let display_name = device.display_name().as_str().to_owned();
            Some(MediaDeviceInfo {
                id: 0,
                device_id: display_name.clone(),
                kind: MediaDeviceKind::VideoInput,
                label: display_name,
            })
        })
        .collect::<Vec<_>>();

    // On macOS, manually add screen capture devices
    if cfg!(target_os = "macos") {
        for i in 1..4 {
            devices.push(MediaDeviceInfo {
                id: i,
                device_id: i.to_string(),
                kind: MediaDeviceKind::VideoInput,
                label: format!("Screen {}", i),
            });
        }
    }

    device_monitor.stop();
    Ok(devices)
}

impl ScreenCapApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Initialize GStreamer
        if let Err(e) = gst::init() {
            eprintln!("Failed to initialize GStreamer: {}", e);
            std::process::exit(1);
        }

        match setup_gstreamer(0) {
            Ok((receiver, sender, width, height, pipeline, gstreamer_devices)) => Self {
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
            },
            Err(e) => {
                eprintln!("Failed to setup GStreamer pipeline: {:?}", e);
                // Return a default app state that shows an error message
                let dummy_pipeline = gst::parse::launch("fakesrc ! fakesink").unwrap();
                Self {
                    gstreamer_devices: vec![],
                    texture: None,
                    receiver: mpsc::channel().1, // dummy receiver
                    sender: mpsc::channel().0,   // dummy sender
                    width: 1280,
                    height: 720,
                    is_recording: false,
                    pipeline: dummy_pipeline.downcast::<gst::Pipeline>().unwrap(),
                    recording_bin: None,
                    current_frame: None,
                    current_device_id: String::new(),
                    show_settings: true,
                    image_size: egui::Vec2::new(1280.0, 720.0),
                }
            }
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

    fn switch_source(&mut self, device_id: u32) {
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
                self.current_device_id = device_id.to_string();
            }
            Err(e) => {
                eprintln!("Failed to start pipeline: {:?}", e);
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
        // Set dark theme with custom colors
        ctx.set_visuals(egui::Visuals::dark());

        // Process frames as before
        while let Ok(buffer) = self.receiver.try_recv() {
            self.current_frame = Some(buffer);
        }

        if let Some(buffer) = &self.current_frame {
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [self.width as usize, self.height as usize],
                buffer,
            );

            self.texture = Some(ctx.load_texture(
                "screen-capture",
                color_image,
                egui::TextureOptions::default(),
            ));
        }

        // Top panel with gradient background
        egui::TopBottomPanel::top("top_panel")
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(30, 30, 40))
                    .inner_margin(10.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new("Screen Capture")
                            .size(20.0)
                            .strong()
                            .color(egui::Color32::from_rgb(200, 200, 255)),
                    ));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Settings button with hover effect
                        let settings_btn = ui.add(
                            egui::Button::new(egui::RichText::new("⚙").size(20.0).color(
                                if self.show_settings {
                                    egui::Color32::from_rgb(130, 180, 255)
                                } else {
                                    egui::Color32::LIGHT_GRAY
                                },
                            ))
                            .frame(false),
                        );
                        if settings_btn.clicked() {
                            self.show_settings = !self.show_settings;
                        }

                        // Record button with status color
                        let record_text = if self.is_recording { "⏹" } else { "⏺" };
                        let record_color = if self.is_recording {
                            egui::Color32::from_rgb(255, 100, 100)
                        } else {
                            egui::Color32::from_rgb(100, 255, 100)
                        };

                        let record_btn = ui.add(
                            egui::Button::new(
                                egui::RichText::new(record_text)
                                    .size(20.0)
                                    .color(record_color),
                            )
                            .frame(false),
                        );

                        if record_btn.clicked() {
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
                            } else {
                                if let Some(tee) = self.pipeline.by_name("t") {
                                    tee.set_property("allow-not-linked", false);
                                }
                            }
                        }
                    });
                });
            });

        // Right settings panel with styled background
        egui::SidePanel::right("settings_panel")
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(35, 35, 45))
                    .inner_margin(10.0),
            )
            .default_width(200.0)
            .show_animated(ctx, self.show_settings, |ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new("Settings")
                        .size(18.0)
                        .strong()
                        .color(egui::Color32::from_rgb(200, 200, 255)),
                ));
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(8.0);

                ui.label(egui::RichText::new("Source Selection").size(14.0).strong());
                ui.add_space(4.0);

                let mut selected_device = None;
                for device in &self.gstreamer_devices {
                    let is_selected = device.device_id == self.current_device_id;
                    let button = ui.add(
                        egui::Button::new(egui::RichText::new(&device.label).color(
                            if is_selected {
                                egui::Color32::from_rgb(130, 180, 255)
                            } else {
                                egui::Color32::LIGHT_GRAY
                            },
                        ))
                        .fill(if is_selected {
                            egui::Color32::from_rgb(45, 45, 60)
                        } else {
                            egui::Color32::TRANSPARENT
                        }),
                    );
                    if button.clicked() {
                        selected_device = Some(device.id);
                    }
                }

                if let Some(device_id) = selected_device {
                    self.switch_source(device_id);
                }

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                ui.label(egui::RichText::new("Statistics").size(14.0).strong());
                ui.add_space(4.0);
                ui.label(format!("State: {:?}", self.pipeline.current_state()));
            });

        // Main video panel
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(20, 20, 25)))
            .show(ctx, |ui| {
                if let Some(texture) = &self.texture {
                    let available_size = ui.available_size();
                    let aspect_ratio = self.width as f32 / self.height as f32;
                    let mut size = available_size;

                    if available_size.x / available_size.y > aspect_ratio {
                        size.x = available_size.y * aspect_ratio;
                    } else {
                        size.y = available_size.x / aspect_ratio;
                    }

                    ui.centered_and_justified(|ui| {
                        let response = ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(size)
                                .sense(egui::Sense::drag())
                                .rounding(4.0),
                        );

                        if response.dragged() {
                            let delta = response.drag_delta();
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
                        ui.add(egui::Label::new(
                            egui::RichText::new("No video input")
                                .size(24.0)
                                .color(egui::Color32::from_rgb(100, 100, 120)),
                        ));
                    });
                }
            });

        ctx.request_repaint();
    }
}

const MACOS_PIPELINE_STR: &str =
    "avfvideosrc device-index=0 ! videoconvert ! video/x-raw,format=RGBA ! appsink name=appsink";
// const MACOS_PIPELINE_STR: &str = "avfvideosrc device-index=0 ! videoconvert ! video/x-raw,width=1280,height=720,framerate=30/1 ! tee name=t t. ! queue ! osxvideosink t. ! queue ! x264enc tune=zerolatency bitrate=2000 speed-preset=ultrafast !";

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
    let source_element = match std::env::consts::OS {
        "macos" => MACOS_PIPELINE_STR.to_string(),
        "windows" => "d3d11screencapturesrc ! videoscale".to_string(),
        "linux" => "ximagesrc ! videoscale".to_string(),
        _ => return Err(gst::FlowError::Error),
    };

    println!("Creating pipeline: {}", source_element);

    let (sender, receiver) = mpsc::channel();
    let cloned_sender = sender.clone();

    let pipeline = match gst::parse::launch(&source_element) {
        Ok(elem) => elem
            .downcast::<gst::Pipeline>()
            .map_err(|_| gst::FlowError::Error)?,
        Err(e) => {
            eprintln!("Failed to create pipeline: {:?}", e);
            return Err(gst::FlowError::Error);
        }
    };

    // First set to NULL to ensure clean state
    if let Err(e) = pipeline.set_state(gst::State::Null) {
        eprintln!("Failed to set pipeline to NULL: {:?}", e);
        return Err(gst::FlowError::Error);
    }

    let appsink = pipeline
        .by_name("sink")
        .ok_or(gst::FlowError::Error)?
        .downcast::<gstreamer_app::AppSink>()
        .map_err(|_| gst::FlowError::Error)?;

    appsink.set_max_buffers(1);
    appsink.set_drop(true);
    appsink.set_sync(false);

    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let _ = sender.send(map.to_vec());
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    // Set to READY first
    if let Err(e) = pipeline.set_state(gst::State::Ready) {
        eprintln!("Failed to set pipeline to READY: {:?}", e);
        return Err(gst::FlowError::Error);
    }

    // Then set to PLAYING
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        eprintln!("Failed to set pipeline to PLAYING: {:?}", e);
        // Try to get more detailed error information
        if let Some(msg) = pipeline.bus().unwrap().timed_pop(gst::ClockTime::NONE) {
            eprintln!("Pipeline error message: {:?}", msg);
        }
        return Err(gst::FlowError::Error);
    }

    // Wait for the pipeline to preroll
    std::thread::sleep(std::time::Duration::from_millis(500));

    let caps = match appsink.static_pad("sink") {
        Some(pad) => pad.current_caps(),
        None => {
            eprintln!("Failed to get sink pad");
            return Err(gst::FlowError::Error);
        }
    }
    .ok_or(gst::FlowError::Error)?;

    let s = caps.structure(0).ok_or(gst::FlowError::Error)?;
    let width = s.get::<i32>("width").map_err(|_| gst::FlowError::Error)?;
    let height = s.get::<i32>("height").map_err(|_| gst::FlowError::Error)?;

    println!("Pipeline created with dimensions: {}x{}", width, height);

    let devices = get_devices().unwrap_or_else(|_| vec![]);
    Ok((receiver, cloned_sender, width, height, pipeline, devices))
}

fn main() -> Result<(), eframe::Error> {
    // Set environment variables to disable most logging
    std::env::set_var("G_MESSAGES_DEBUG", "none");
    std::env::set_var("GST_DEBUG", "none,GST_ELEMENT_FACTORY:0");
    std::env::set_var("GST_REGISTRY_UPDATE", "no");
    std::env::set_var("GST_REGISTRY_FORK", "no");

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
