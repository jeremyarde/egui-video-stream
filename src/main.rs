use chrono;
use core_graphics::display::{CGDisplay, CGDisplayBounds};
use eframe::egui;
use egui::epaint::text::layout;
use egui::FontId;
use egui::ViewportBuilder;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::Cast;
use gstreamer::prelude::*;
use gstreamer::DeviceMonitor;
use gstreamer::{prelude::*, DeviceMonitorFilterId};
use gstreamer_app;
use gstreamer_audio;
use std::sync::{mpsc, Arc, Mutex};
use sysinfo::System;
use tracing::debug;

// Constants for pipeline strings
const CAMERA_PIPELINE: &str = "avfvideosrc device-index=0 ! video/x-raw,width=1280,height=720,framerate=30/1 ! videoconvert ! video/x-raw,format=RGBA,width=1280,height=720 ! queue leaky=downstream max-size-buffers=1 ! appsink name=sink sync=false drop=true max-buffers=1 emit-signals=true";
const SCREEN_PIPELINE: &str = "avfvideosrc capture-screen=true capture-screen-cursor=true device-index={} ! videoconvert ! video/x-raw,format=RGBA,framerate=30/1 ! queue leaky=downstream max-size-buffers=1 ! appsink name=sink sync=false drop=true max-buffers=1 emit-signals=true";
const RECORDING_PIPELINE: &str = "
    matroskamux name=mux ! filesink name=filesink sync=false
    appsrc name=video_src format=time is-live=true ! videoconvert ! x264enc tune=zerolatency ! h264parse ! queue ! mux.
    osxaudiosrc ! audioconvert ! audioresample ! audio/x-raw,rate=44100 ! queue ! avenc_aac ! aacparse ! queue ! mux.
";

const GEAR_ICON: &str = "\u{f0e6}";
const FULLSCREEN_ICON: &str = "\u{ed9b}";
const FULLSCREEN_EXIT_ICON: &str = "\u{ed9a}";
const RECORD_ON_ICON: &str = "\u{F059}";
const RECORD_OFF_ICON: &str = "\u{F05A}";
const MIC_ON_ICON: &str = "\u{EF50}";
const MIC_OFF_ICON: &str = "\u{EF52}";

struct ScreenCapApp {
    texture: Option<egui::TextureHandle>,
    frame_data: Arc<Mutex<Option<Vec<u8>>>>,
    dimensions: Arc<Mutex<ImageDimensions>>,
    is_recording: bool,
    is_mic_enabled: bool,
    pipeline: gst::Pipeline,
    recording_bin: Option<gst::Element>,
    current_device_idx: Option<usize>,
    current_mic_idx: Option<usize>,
    audio_devices: Vec<MediaDeviceInfo>,
    video_devices: Vec<MediaDeviceInfo>,
    show_settings: bool,
    image_size: egui::Vec2,
    update_dimensions_tx: mpsc::Sender<bool>,
    update_audio_tx: mpsc::Sender<bool>,
    audio_bin: Option<gst::Element>,
    recording_files: Option<(String, String, String)>, // (video_file, audio_file, final_file)
    is_fullscreen: bool,
}

#[derive(Debug)]
struct MediaDeviceInfo {
    pipeline_id: u32,
    kind: MediaDeviceKind,
    label: String,
    setup_pipeline: String,
    device_id: Option<String>, // For audio devices
}

#[derive(Debug, PartialEq)]
enum MediaDeviceKind {
    AudioInput,
    AudioOutput,
    VideoInput,
}

impl ScreenCapApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Initialize GStreamer
        if let Err(e) = gst::init() {
            eprintln!("Failed to initialize GStreamer: {}", e);
            std::process::exit(1);
        }

        // Load custom fonts
        add_font(&cc.egui_ctx);

        // Get audio devices
        let audio_devices = get_audio_devices();
        let current_mic_idx = if !audio_devices.is_empty() {
            Some(0)
        } else {
            None
        };

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
                    audio_devices,
                    video_devices: devices,
                    update_audio_tx: mpsc::channel().0,
                    texture: None,
                    frame_data,
                    dimensions: image_dims,
                    update_dimensions_tx: tx,
                    is_recording: false,
                    is_mic_enabled: true,
                    current_mic_idx,
                    pipeline,
                    recording_bin: None,
                    current_device_idx: Some(0),
                    show_settings: false,
                    image_size,
                    audio_bin: None,
                    recording_files: None,
                    is_fullscreen: false,
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
                    audio_devices,
                    video_devices: vec![],
                    texture: None,
                    frame_data: Arc::new(Mutex::new(None)),
                    dimensions: default_dims,
                    is_recording: false,
                    is_mic_enabled: true,
                    current_mic_idx: None,
                    pipeline: dummy_pipeline.downcast::<gst::Pipeline>().unwrap(),
                    recording_bin: None,
                    current_device_idx: Some(0),
                    show_settings: false,
                    image_size: egui::Vec2::new(1280.0, 720.0),
                    update_dimensions_tx: mpsc::channel().0,
                    update_audio_tx: mpsc::channel().0,
                    audio_bin: None,
                    recording_files: None,
                    is_fullscreen: false,
                }
            }
        }
    }

    fn start_recording(&mut self) {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let filename = format!("recording_{}.mkv", timestamp);

        // Create recording pipeline with selected audio device
        let audio_pipeline = if self.is_mic_enabled {
            // Default audio pipeline without specific device
            "osxaudiosrc ! audioconvert ! audioresample ! audio/x-raw,rate=44100 ! queue ! avenc_aac ! aacparse ! queue ! mux.".to_string()
        } else {
            String::new()
        };

        // Create recording pipeline
        let recording_pipeline = format!(
            "appsrc name=video_src format=time is-live=true do-timestamp=true ! videoconvert ! video/x-raw,format=I420 ! x264enc tune=zerolatency ! h264parse ! queue ! mux. {} matroskamux name=mux ! filesink name=filesink sync=false location={}",
            audio_pipeline,
            filename
        );

        println!("Creating recording pipeline: {}", recording_pipeline);

        match gst::parse::launch(&recording_pipeline) {
            Ok(elem) => {
                let recording_bin = elem.downcast::<gst::Pipeline>().unwrap();

                // Set up video source
                if let Some(video_src) = recording_bin.by_name("video_src") {
                    let dims = self.dimensions.lock().unwrap();
                    let caps = gst::Caps::builder("video/x-raw")
                        .field("format", "RGBA")
                        .field("width", dims.width)
                        .field("height", dims.height)
                        .field("framerate", gst::Fraction::new(30, 1))
                        .build();
                    video_src.set_property("caps", &caps);

                    // Set up buffer handling
                    let video_src = video_src
                        .downcast::<gstreamer_app::AppSrc>()
                        .expect("Failed to downcast to AppSrc");

                    video_src.set_format(gst::Format::Time);
                    video_src.set_max_bytes(1);
                    video_src.set_do_timestamp(true);

                    let frame_data = self.frame_data.clone();
                    let video_src_weak = video_src.downgrade();
                    video_src.set_callbacks(
                        gstreamer_app::AppSrcCallbacks::builder()
                            .need_data(move |_, _| {
                                if let Some(video_src) = video_src_weak.upgrade() {
                                    if let Ok(guard) = frame_data.lock() {
                                        if let Some(buffer) = guard.as_ref() {
                                            let mut gst_buffer =
                                                gst::Buffer::with_size(buffer.len())
                                                    .expect("Failed to allocate buffer");
                                            {
                                                let buffer_mut = gst_buffer.get_mut().unwrap();
                                                let mut data = buffer_mut.map_writable().unwrap();
                                                data.copy_from_slice(buffer);
                                            }
                                            let _ = video_src.push_buffer(gst_buffer);
                                        }
                                    }
                                }
                            })
                            .build(),
                    );
                }

                // Start the recording pipeline
                if let Err(e) = recording_bin.set_state(gst::State::Playing) {
                    eprintln!("Failed to start recording pipeline: {:?}", e);
                    if let Some(msg) = recording_bin.bus().unwrap().timed_pop(gst::ClockTime::NONE)
                    {
                        eprintln!("Recording pipeline error message: {:?}", msg);
                    }
                } else {
                    println!("Recording started: {}", filename);
                    self.recording_bin = Some(recording_bin.upcast());
                    self.is_recording = true;
                }
            }
            Err(e) => {
                eprintln!("Failed to create recording pipeline: {:?}", e);
            }
        }
    }

    fn stop_recording(&mut self) {
        if let Some(recording_bin) = self.recording_bin.take() {
            if let Err(e) = recording_bin.set_state(gst::State::Null) {
                eprintln!("Error stopping recording pipeline: {:?}", e);
            }
        }
        self.is_recording = false;
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

    fn switch_source(&mut self, device_idx: usize) {
        // Stop the current pipeline first
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            eprintln!("Error stopping pipeline: {:?}", e);
        }

        // Start the new pipeline with error handling
        match setup_gstreamer(device_idx) {
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
                self.video_devices = devices;
                self.update_dimensions_tx = tx;
                self.current_device_idx = Some(device_idx);

                // Update image size
                let dims = self.dimensions.lock().unwrap();
                self.image_size = egui::Vec2::new(dims.width as f32, dims.height as f32);
                println!("Switched to device {}", device_idx);
            }
            Err(e) => {
                eprintln!("Failed to start pipeline: {:?}", e);
            }
        }
    }

    fn switch_mic(&mut self, idx: usize) {
        self.current_mic_idx = Some(idx);
        if self.is_recording {
            self.stop_recording();
            self.start_recording();
        }
    }

    fn current_device_label(&self) -> String {
        self.current_device_idx
            .and_then(|idx| self.video_devices.get(idx))
            .map(|device| device.label.clone())
            .unwrap_or_else(|| "FaceTime Camera".to_string())
    }

    fn current_mic_label(&self) -> String {
        self.current_mic_idx
            .and_then(|idx| self.audio_devices.get(idx))
            .map(|device| device.label.clone())
            .unwrap_or_else(|| "Default Microphone".to_string())
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
        // Add keyboard shortcuts
        if ctx.input(|i| i.modifiers.command) {
            if ctx.input(|i| i.key_pressed(egui::Key::R)) {
                // Cmd+R to start/stop recording
                if self.is_recording {
                    self.stop_recording();
                } else {
                    self.start_recording();
                }
            }
        }

        // Set dark theme with custom colors
        ctx.set_visuals(egui::Visuals::dark());

        // Process frame data and update texture
        if let Ok(frame_guard) = self.frame_data.lock() {
            if let Some(buffer) = frame_guard.as_ref() {
                let dims = self.dimensions.lock().unwrap();
                let expected_size = (dims.width * dims.height * 4) as usize;

                if buffer.len() == expected_size {
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [dims.width as usize, dims.height as usize],
                        buffer,
                    );

                    self.texture = Some(ctx.load_texture(
                        format!("screen-capture-{:?}", self.current_device_idx),
                        color_image,
                        egui::TextureOptions::default(),
                    ));
                }
            }
        }

        // Main video panel as background
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(20, 20, 25)))
            .show(ctx, |ui| {
                if let Some(texture) = &self.texture {
                    let available_size = ui.available_size();
                    let dims = self.dimensions.lock().unwrap();
                    let aspect_ratio = dims.width as f32 / dims.height as f32;
                    drop(dims);
                    let mut size = available_size;

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

        // Settings button in the corner
        if !self.show_settings {
            egui::Window::new("")
                .resizable(false)
                .collapsible(false)
                .title_bar(false)
                .movable(true)
                .fixed_pos(egui::pos2(20.0, 20.0))
                .frame(
                    egui::Frame::window(&egui::Style::default())
                        .fill(egui::Color32::from_rgba_premultiplied(20, 20, 30, 250))
                        .rounding(12.0),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(GEAR_ICON).font(FontId::proportional(18.0)),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            println!("Show settings - show_settings - {}", self.show_settings); // self.show_settings = !self.show_settings;
                            if self.show_settings {
                                self.show_settings = true;
                            } else {
                                self.show_settings = true;
                            }
                        }

                        // Record button
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(if self.is_recording {
                                        RECORD_ON_ICON
                                    } else {
                                        RECORD_OFF_ICON
                                    })
                                    .font(FontId::proportional(18.0))
                                    .color(
                                        if self.is_recording {
                                            egui::Color32::from_rgb(255, 80, 80)
                                        } else {
                                            egui::Color32::LIGHT_GRAY
                                        },
                                    ),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            if self.is_recording {
                                self.stop_recording();
                            } else {
                                self.start_recording();
                            }
                        }

                        // Fullscreen button
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(if self.is_fullscreen {
                                        FULLSCREEN_EXIT_ICON
                                    } else {
                                        FULLSCREEN_ICON
                                    })
                                    .font(FontId::proportional(18.0)),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            self.is_fullscreen = !self.is_fullscreen;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(
                                self.is_fullscreen,
                            ));
                        }
                    });
                });
        }

        // Floating settings panel
        if self.show_settings {
            egui::Window::new("") // Empty title
                .resizable(false)
                .collapsible(false)
                .title_bar(false)
                .movable(true)
                .default_pos(egui::pos2(20.0, 20.0))
                .frame(
                    egui::Frame::window(&egui::Style::default())
                        .fill(egui::Color32::from_rgba_premultiplied(20, 20, 30, 250))
                        .rounding(12.0)
                        .outer_margin(0.0)
                        .inner_margin(12.0),
                )
                .show(ctx, |ui| {
                    // Add close button and controls in the top bar
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(GEAR_ICON).font(FontId::proportional(18.0)),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            println!(
                                "Show settings - show_settings (SKIPPED) - {}",
                                self.show_settings
                            );

                            self.show_settings = false;
                        }

                        // Record button
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(if self.is_recording {
                                        RECORD_ON_ICON
                                    } else {
                                        RECORD_OFF_ICON
                                    })
                                    .font(FontId::proportional(18.0))
                                    .color(
                                        if self.is_recording {
                                            egui::Color32::from_rgb(255, 80, 80)
                                        } else {
                                            egui::Color32::LIGHT_GRAY
                                        },
                                    ),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            if self.is_recording {
                                self.stop_recording();
                            } else {
                                self.start_recording();
                            }
                        }

                        // Fullscreen button
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(if self.is_fullscreen {
                                        FULLSCREEN_EXIT_ICON
                                    } else {
                                        FULLSCREEN_ICON
                                    })
                                    .font(FontId::proportional(18.0)),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            self.is_fullscreen = !self.is_fullscreen;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(
                                self.is_fullscreen,
                            ));
                        }
                    });

                    ui.add_space(12.0);

                    // Source selection with modern style
                    ui.label(
                        egui::RichText::new("Video Source")
                            .size(13.0)
                            .color(egui::Color32::from_rgb(180, 180, 180)),
                    );

                    let mut selected_video_src_idx = None;
                    ui.horizontal(|ui| {
                        let current_label = self
                            .current_device_idx
                            .and_then(|idx| self.video_devices.get(idx))
                            .map(|device| device.label.as_str())
                            .unwrap_or("Default");

                        egui::ComboBox::from_id_salt("source_select")
                            .selected_text(current_label)
                            .width(ui.available_width() - 40.0)
                            .show_ui(ui, |ui| {
                                for (idx, device) in self.video_devices.iter().enumerate() {
                                    let selected = Some(idx) == self.current_device_idx;
                                    if ui.selectable_label(selected, &device.label).clicked()
                                        && !selected
                                    {
                                        selected_video_src_idx = Some(idx);
                                    }
                                }
                            });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let is_on = self.current_device_idx == Some(0);
                            ui.add(
                                egui::Button::new(
                                    egui::RichText::new(if is_on { "On" } else { "Off" })
                                        .size(12.0)
                                        .color(if is_on {
                                            egui::Color32::from_rgb(100, 255, 100)
                                        } else {
                                            egui::Color32::LIGHT_GRAY
                                        }),
                                )
                                .frame(false),
                            );
                        });
                    });

                    ui.add_space(12.0);

                    // Audio selection with modern style
                    ui.label(
                        egui::RichText::new("Audio Source")
                            .size(13.0)
                            .color(egui::Color32::from_rgb(180, 180, 180)),
                    );

                    let mut selected_mic_idx = None;
                    ui.horizontal(|ui| {
                        let current_label = self
                            .current_mic_idx
                            .and_then(|idx| self.audio_devices.get(idx))
                            .map(|device| device.label.as_str())
                            .unwrap_or("Default");

                        egui::ComboBox::from_id_salt("mic_select")
                            .selected_text(current_label)
                            .width(ui.available_width() - 40.0)
                            .show_ui(ui, |ui| {
                                for (idx, device) in self.audio_devices.iter().enumerate() {
                                    let selected = Some(idx) == self.current_mic_idx;
                                    if ui.selectable_label(selected, &device.label).clicked()
                                        && !selected
                                    {
                                        selected_mic_idx = Some(idx);
                                    }
                                }
                            });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let mut is_enabled = self.is_mic_enabled;
                            ui.checkbox(&mut is_enabled, "");
                            if is_enabled != self.is_mic_enabled {
                                self.is_mic_enabled = is_enabled;
                                if self.is_recording {
                                    self.stop_recording();
                                    self.start_recording();
                                }
                            }
                        });
                    });

                    ui.add_space(16.0);

                    // Start recording button with modern style
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 120.0) / 2.0); // Center the button
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(if self.is_recording {
                                        "Stop Recording"
                                    } else {
                                        "Start Recording"
                                    })
                                    .size(13.0)
                                    .color(egui::Color32::WHITE),
                                )
                                .min_size(egui::vec2(120.0, 28.0))
                                .fill(if self.is_recording {
                                    egui::Color32::from_rgb(220, 60, 60)
                                } else {
                                    egui::Color32::from_rgb(240, 80, 80)
                                })
                                .rounding(6.0),
                            )
                            .clicked()
                        {
                            if self.is_recording {
                                self.stop_recording();
                            } else {
                                self.start_recording();
                            }
                        }
                    });

                    ui.add_space(16.0);

                    // Handle source switching outside the UI closure
                    if let Some(idx) = selected_video_src_idx {
                        self.switch_source(idx);
                    }
                    if let Some(idx) = selected_mic_idx {
                        self.switch_mic(idx);
                    }
                });
        }

        // Request continuous repaints for smooth video
        ctx.request_repaint();
    }
}

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

fn setup_gstreamer(device_idx: usize) -> Result<GstreamerSetup, anyhow::Error> {
    let displays = CGDisplay::active_displays().expect("Failed to get active displays");
    println!("Found {} displays", displays.len());

    // Create devices list starting with FaceTime camera
    let mut devices = vec![MediaDeviceInfo {
        pipeline_id: 0,
        kind: MediaDeviceKind::VideoInput,
        label: "FaceTime Camera".to_string(),
        setup_pipeline: CAMERA_PIPELINE.to_string(),
        device_id: None,
    }];

    // Add displays
    for (i, display_id) in displays.iter().enumerate() {
        let bounds = unsafe { CGDisplayBounds(*display_id) };
        devices.push(MediaDeviceInfo {
            pipeline_id: (i + 1) as u32,
            kind: MediaDeviceKind::VideoInput,
            label: format!(
                "Display {} ({}x{})",
                i + 1,
                bounds.size.width,
                bounds.size.height
            ),
            setup_pipeline: SCREEN_PIPELINE.replace("{}", &i.to_string()),
            device_id: None,
        });
    }

    println!("Available devices:");
    for (i, device) in devices.iter().enumerate() {
        println!("Device {}: {}", i, device.label);
    }

    println!("Setting up pipeline for device index: {}", device_idx);
    if device_idx >= devices.len() {
        return Err(anyhow::anyhow!("Invalid device index"));
    }

    let selected_device = &devices[device_idx];
    println!("Selected device: {:?}", selected_device);
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

fn get_audio_devices() -> Vec<MediaDeviceInfo> {
    let mut devices = Vec::new();
    let monitor = DeviceMonitor::new();

    monitor.set_show_all_devices(true);
    let _ = monitor.start();

    // Get devices
    let device_list = monitor.devices();
    for device in device_list {
        // Only include audio input devices (microphones)
        if device.device_class().contains("Audio/Source") {
            // Get the actual device ID from properties
            // This is for audio input devices (microphones)
            let device_id = if let Some(props) = device.properties() {
                props
                    .get::<String>("device.id")
                    .ok()
                    .or_else(|| Some(device.display_name().to_string()))
            } else {
                None
            };

            devices.push(MediaDeviceInfo {
                pipeline_id: devices.len() as u32,
                kind: MediaDeviceKind::AudioInput,
                label: device.display_name().to_string(),
                setup_pipeline: String::new(),
                device_id,
            });
        }
    }

    monitor.stop();

    // If no devices were found, add a default device
    if devices.is_empty() {
        devices.push(MediaDeviceInfo {
            pipeline_id: 0,
            kind: MediaDeviceKind::AudioInput,
            label: "Default Microphone".to_string(),
            setup_pipeline: String::new(),
            device_id: None,
        });
    }

    println!("Found {} audio input devices: {:?}", devices.len(), devices);
    devices
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

use egui::FontData;
use egui::FontDefinitions;

fn add_font(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    // Add FontAwesome
    fonts.font_data.insert(
        "FontAwesome".to_owned(),
        Arc::new(FontData::from_static(include_bytes!("../remixicon.ttf"))),
    );

    // Insert it into the `proportional` or `monospace` font family
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "FontAwesome".to_owned());

    ctx.set_fonts(fonts);
}
