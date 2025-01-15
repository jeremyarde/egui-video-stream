use chrono;
use eframe::egui;
use egui::ViewportBuilder;
use gstreamer::glib;
use gstreamer::prelude::Cast;
use gstreamer::{self as gst, DeviceMonitor};
use gstreamer::{prelude::*, DeviceMonitorFilterId};
use gstreamer_app;
use std::sync::{mpsc, Arc, Mutex};
use tracing::debug;

struct ScreenCapApp {
    texture: Option<egui::TextureHandle>,
    frame_data: Arc<Mutex<Option<Vec<u8>>>>,
    dimensions: Arc<Mutex<ImageDimensions>>,
    is_recording: bool,
    pipeline: gst::Pipeline,
    recording_bin: Option<gst::Element>,
    current_device_id: String,
    gstreamer_devices: Vec<MediaDeviceInfo>,
    show_settings: bool,
    image_size: egui::Vec2,
    update_dimensions_tx: mpsc::Sender<bool>,
}

#[derive(Debug)]
struct MediaDeviceInfo {
    id: u32,
    device_id: String,
    kind: MediaDeviceKind,
    label: String,
    setup_pipeline: String,
}

#[derive(Debug, PartialEq)]
enum MediaDeviceKind {
    AudioInput,
    AudioOutput,
    VideoInput,
}

fn get_devices() -> Result<Vec<MediaDeviceInfo>, ()> {
    let mut devices = Vec::new();

    // Always add FaceTime camera as device 0
    devices.push(MediaDeviceInfo {
        id: 0,
        device_id: "0".to_string(),
        kind: MediaDeviceKind::VideoInput,
        label: "FaceTime Camera".to_string(),
        setup_pipeline: CAMERA_PIPELINE.to_string(),
    });

    // Add screen capture devices on macOS
    if cfg!(target_os = "macos") {
        // Add main display
        devices.push(MediaDeviceInfo {
            id: 1,
            device_id: "1".to_string(),
            kind: MediaDeviceKind::VideoInput,
            label: "Main Display".to_string(),
            setup_pipeline: SCREEN_PIPELINE.replace("{}", "0"),
        });

        // Try to detect additional displays
        for i in 2..4 {
            devices.push(MediaDeviceInfo {
                id: i,
                device_id: i.to_string(),
                kind: MediaDeviceKind::VideoInput,
                label: format!("Display {}", i),
                setup_pipeline: SCREEN_PIPELINE.replace("{}", &(i - 1).to_string()),
            });
        }
    }

    println!("Available devices: {:?}", devices);
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
            Ok(GstreamerSetup {
                frame_data,
                image_dims,
                pipeline,
                devices,
                tx,
            }) => {
                let width;
                let height;
                {
                    let dims = image_dims.lock().unwrap();
                    width = dims.width;
                    height = dims.height;
                }
                let image_size = egui::Vec2::new(width as f32, height as f32);
                Self {
                    gstreamer_devices: devices,
                    texture: None,
                    frame_data,
                    dimensions: image_dims,
                    update_dimensions_tx: tx,
                    is_recording: false,
                    pipeline,
                    recording_bin: None,
                    current_device_id: String::new(),
                    show_settings: true,
                    image_size,
                }
            }
            Err(err) => {
                eprintln!("Failed to setup GStreamer pipeline: {:?}", err);
                // Return a default app state that shows an error message
                let dummy_pipeline = gst::parse::launch("fakesrc ! fakesink").unwrap();
                let default_dims = Arc::new(Mutex::new(ImageDimensions {
                    width: 1280,
                    height: 720,
                }));
                Self {
                    gstreamer_devices: vec![],
                    texture: None,
                    frame_data: Arc::new(Mutex::new(None)),
                    dimensions: default_dims,
                    is_recording: false,
                    pipeline: dummy_pipeline.downcast::<gst::Pipeline>().unwrap(),
                    recording_bin: None,
                    current_device_id: String::new(),
                    show_settings: true,
                    image_size: egui::Vec2::new(1280.0, 720.0),
                    update_dimensions_tx: mpsc::channel().0,
                }
            }
        }
    }

    pub fn get_current_frame(&self) -> Option<Vec<u8>> {
        self.frame_data.lock().ok()?.as_ref().cloned()
    }

    pub fn get_dimensions(&self) -> (i32, i32) {
        let dims = self.dimensions.lock().unwrap();
        (dims.width, dims.height)
    }

    pub fn get_pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        let dims = self.dimensions.lock().unwrap();
        if x < 0 || x >= dims.width || y < 0 || y >= dims.height {
            return None;
        }

        self.frame_data.lock().unwrap().as_ref().map(|frame| {
            let idx = ((y * dims.width + x) * 4) as usize;
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
            Ok(GstreamerSetup {
                frame_data,
                image_dims,
                pipeline,
                devices,
                tx,
            }) => {
                self.frame_data = frame_data;
                self.dimensions = image_dims;
                self.pipeline = pipeline;
                self.gstreamer_devices = devices;
                self.update_dimensions_tx = tx;
                self.current_device_id = device_id.to_string();

                // Update image size
                let dims = self.dimensions.lock().unwrap();
                self.image_size = egui::Vec2::new(dims.width as f32, dims.height as f32);
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

        // Process frame data and update texture
        if let Ok(frame_guard) = self.frame_data.lock() {
            if let Some(buffer) = frame_guard.as_ref() {
                let dims = self.dimensions.lock().unwrap();
                let expected_size = (dims.width * dims.height * 4) as usize;

                if buffer.len() == expected_size {
                    // Create color image from RGBA buffer
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [dims.width as usize, dims.height as usize],
                        buffer,
                    );

                    // Update texture with appropriate options
                    let options = egui::TextureOptions {
                        magnification: egui::TextureFilter::Linear,
                        minification: egui::TextureFilter::Linear,
                        ..Default::default()
                    };

                    // Only update texture if dimensions match
                    self.texture = Some(ctx.load_texture(
                        format!("screen-capture-{}", self.current_device_id),
                        color_image,
                        options,
                    ));
                } else {
                    println!(
                        "Buffer size mismatch: got {}, expected {} ({}x{})",
                        buffer.len(),
                        expected_size,
                        dims.width,
                        dims.height
                    );
                }
            }
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
                    let dims = self.dimensions.lock().unwrap();
                    let aspect_ratio = dims.width as f32 / dims.height as f32;
                    drop(dims); // Release the lock
                    let mut size = available_size;

                    // Calculate size to maintain aspect ratio
                    if available_size.x / available_size.y > aspect_ratio {
                        size.x = available_size.y * aspect_ratio;
                    } else {
                        size.y = available_size.x / aspect_ratio;
                    }

                    ui.centered_and_justified(|ui| {
                        ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(size)
                                .sense(egui::Sense::drag())
                                .rounding(4.0),
                        );
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

        // Request continuous repaints for smooth video
        ctx.request_repaint();
    }
}

// Constants for pipeline strings
const CAMERA_PIPELINE: &str = "avfvideosrc device-index=0 ! video/x-raw,width=1280,height=720,framerate=30/1 ! videoconvert ! video/x-raw,format=RGBA,width=1280,height=720 ! queue leaky=downstream max-size-buffers=1 ! appsink name=sink sync=false drop=true max-buffers=1 emit-signals=true";
const SCREEN_PIPELINE: &str = "avfvideosrc capture-screen=true capture-screen-cursor=true device-index={} ! videoconvert ! video/x-raw,format=RGBA,framerate=30/1 ! queue leaky=downstream max-size-buffers=1 ! appsink name=sink sync=false drop=true max-buffers=1 emit-signals=true";

struct ImageDimensions {
    width: i32,
    height: i32,
}

struct GstreamerSetup {
    frame_data: Arc<Mutex<Option<Vec<u8>>>>,
    image_dims: Arc<Mutex<ImageDimensions>>,
    pipeline: gst::Pipeline,
    devices: Vec<MediaDeviceInfo>,
    tx: mpsc::Sender<bool>,
}

fn setup_gstreamer(device_id: u32) -> Result<GstreamerSetup, anyhow::Error> {
    // First try to get available devices
    let devices = get_devices().unwrap_or_else(|_| vec![]);

    // Find the selected device
    let selected_device = devices
        .iter()
        .find(|d| d.id == device_id)
        .ok_or_else(|| anyhow::anyhow!("Device not found"))?;

    println!("Setting up pipeline for device: {:?}", selected_device);
    println!("Using pipeline: {}", selected_device.setup_pipeline);

    let pipeline = gst::parse::launch(&selected_device.setup_pipeline)
        .map_err(|e| anyhow::anyhow!("Failed to create pipeline: {:?}", e))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("Failed to downcast to Pipeline"))?;

    let frame_data = Arc::new(Mutex::new(None));
    let frame_data_clone = frame_data.clone();

    let appsink = pipeline
        .by_name("sink")
        .ok_or(anyhow::anyhow!("Failed to find sink"))?
        .downcast::<gstreamer_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("Failed to cast to AppSink"))?;

    // Configure appsink
    appsink.set_max_buffers(1);
    appsink.set_drop(true);
    appsink.set_sync(false);

    let image_dims = ImageDimensions {
        width: 0,
        height: 0,
    };

    let image_dims_clone = Arc::new(Mutex::new(image_dims));
    let image_dims_for_callback = image_dims_clone.clone();

    // channel to ask for updated dimensions
    let (tx, rx) = mpsc::channel::<bool>();
    // ask for updated dimensions
    tx.send(true).unwrap();

    // Set up callbacks before starting the pipeline
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| {
                    println!("Failed to pull sample");
                    gst::FlowError::Error
                })?;

                if rx.try_recv().is_ok() {
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    println!("Caps: {:?}", caps);
                    let mut dims = image_dims_for_callback.lock().unwrap();

                    println!("Received message from channel");

                    if let Some(s) = caps.structure(0) {
                        dims.width = s.get::<i32>("width").unwrap_or(1280);
                        dims.height = s.get::<i32>("height").unwrap_or(720);
                        println!(
                            "Updated dimensions from caps: {}x{}",
                            dims.width, dims.height
                        );
                    } else {
                        println!("No structure in caps, using default dimensions");
                        dims.width = 1280;
                        dims.height = 720;
                    }
                }

                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                let mut data = frame_data_clone.lock().unwrap();
                *data = Some(map.as_ref().to_vec());

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    println!("Pipeline callbacks set");

    // Set to PLAYING
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        eprintln!("Failed to set pipeline to PLAYING: {:?}", e);
        if let Some(msg) = pipeline.bus().unwrap().timed_pop(gst::ClockTime::NONE) {
            eprintln!("Pipeline error message: {:?}", msg);
        }
        return Err(anyhow::anyhow!(
            "Failed to set pipeline to PLAYING: {:?}",
            e
        ));
    }

    println!("Pipeline set to PLAYING");

    // Wait a bit for the pipeline to start
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Get the current dimensions and update if needed
    {
        let mut dims = image_dims_clone.lock().unwrap();
        if dims.width == 0 || dims.height == 0 {
            dims.width = 1280;
            dims.height = 720;
        }
        println!(
            "Pipeline created with dimensions: {}x{}",
            dims.width, dims.height
        );
    } // Lock is released here

    Ok(GstreamerSetup {
        frame_data,
        image_dims: image_dims_clone,
        pipeline,
        devices,
        tx,
    })
}

fn main() -> Result<(), eframe::Error> {
    // Set environment variables to disable most logging
    // std::env::set_var("G_MESSAGES_DEBUG", "none");
    // std::env::set_var("GST_DEBUG", "none,GST_ELEMENT_FACTORY:0");
    // std::env::set_var("GST_REGISTRY_UPDATE", "no");
    // std::env::set_var("GST_REGISTRY_FORK", "no");

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
