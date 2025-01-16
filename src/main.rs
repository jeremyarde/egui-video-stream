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
const SCREEN_PIPELINE: &str = "avfvideosrc capture-screen=true capture-screen-cursor=true device-index={} ! videoconvert ! video/x-raw,format=RGBA,framerate=60/1 ! queue leaky=downstream max-size-buffers=1 ! appsink name=sink sync=false drop=true max-buffers=1 emit-signals=true";
const RECORDING_PIPELINE: &str = "
    matroskamux name=mux ! filesink name=filesink sync=false
    appsrc name=video_src format=time is-live=true ! videoconvert ! x264enc tune=zerolatency ! h264parse ! queue ! mux.
    osxaudiosrc ! audioconvert ! audioresample ! audio/x-raw,rate=44100,channels=2 ! queue ! avenc_aac ! aacparse ! queue ! mux.
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
    // PiP state
    show_pip: bool,
    pip_texture: Option<egui::TextureHandle>,
    pip_frame_data: Arc<Mutex<Option<Vec<u8>>>>,
    pip_dimensions: Arc<Mutex<ImageDimensions>>,
    pip_pipeline: Option<gst::Pipeline>,
    pip_position: egui::Pos2,
    pip_size: egui::Vec2,
    pip_desired_size: egui::Vec2,
    recording_path: std::path::PathBuf,
    main_pipeline: Option<gst::Pipeline>,
    recording_pipeline: Option<gst::Pipeline>,
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

        let recording_path = std::path::PathBuf::from(format!(
            "recording_{}.mp4",
            chrono::Local::now().format("%Y%m%d_%H%M%S")
        ));

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
                    // PiP state
                    show_pip: false,
                    pip_texture: None,
                    pip_frame_data: Arc::new(Mutex::new(None)),
                    pip_dimensions: Arc::new(Mutex::new(ImageDimensions {
                        width: 0,
                        height: 0,
                    })),
                    pip_pipeline: None,
                    pip_position: egui::Pos2::default(),
                    pip_size: egui::Vec2::default(),
                    pip_desired_size: egui::vec2(320.0, 240.0),
                    recording_path,
                    main_pipeline: None,
                    recording_pipeline: None,
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
                    // PiP state
                    show_pip: false,
                    pip_texture: None,
                    pip_frame_data: Arc::new(Mutex::new(None)),
                    pip_dimensions: Arc::new(Mutex::new(ImageDimensions {
                        width: 0,
                        height: 0,
                    })),
                    pip_pipeline: None,
                    pip_position: egui::Pos2::default(),
                    pip_size: egui::Vec2::default(),
                    pip_desired_size: egui::vec2(320.0, 240.0),
                    recording_path,
                    main_pipeline: None,
                    recording_pipeline: None,
                }
            }
        }
    }

    fn start_recording(&mut self) -> Result<(), anyhow::Error> {
        // Create unique filenames for the recording
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let main_video = format!("recording_{}_main.mkv", timestamp);
        let pip_video = format!("recording_{}_pip.mkv", timestamp);

        // Create main video recording pipeline with high quality settings
        let main_pipeline_str = format!(
            "appsrc name=video_src format=time is-live=true do-timestamp=true ! \
             videoconvert ! video/x-raw,format=I420 ! \
             x264enc tune=zerolatency speed-preset=slower bitrate=8000 key-int-max=60 ! \
             matroskamux name=mux ! filesink location={} \
             osxaudiosrc ! audioconvert ! audioresample ! \
             audio/x-raw,rate=44100,channels=2 ! \
             avenc_aac bitrate=320000 ! queue ! mux.",
            main_video
        );

        println!("Using main pipeline: {}", main_pipeline_str);

        let main_pipeline = gst::parse::launch(&main_pipeline_str)
            .map_err(|e| anyhow::anyhow!("Failed to create main recording pipeline: {:?}", e))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("Failed to downcast to Pipeline"))?;

        // Set up main video source
        if let Some(video_src) = main_pipeline.by_name("video_src") {
            let video_src = video_src
                .downcast::<gstreamer_app::AppSrc>()
                .map_err(|_| anyhow::anyhow!("Failed to downcast to AppSrc"))?;

            video_src.set_format(gst::Format::Time);
            video_src.set_max_bytes(1);
            video_src.set_do_timestamp(true);

            let dims = self.dimensions.lock().unwrap();
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .field("width", dims.width)
                .field("height", dims.height)
                .field("framerate", gst::Fraction::new(30, 1))
                .build();
            video_src.set_caps(Some(&caps));

            let frame_data = self.frame_data.clone();
            let video_src_weak = video_src.downgrade();

            video_src.set_callbacks(
                gstreamer_app::AppSrcCallbacks::builder()
                    .need_data(move |_, _| {
                        if let Some(src) = video_src_weak.upgrade() {
                            if let Ok(guard) = frame_data.lock() {
                                if let Some(buffer) = guard.as_ref() {
                                    let mut gst_buffer = gst::Buffer::with_size(buffer.len())
                                        .expect("Failed to allocate buffer");
                                    {
                                        let buffer_mut = gst_buffer.get_mut().unwrap();
                                        let mut data = buffer_mut.map_writable().unwrap();
                                        data.copy_from_slice(buffer);
                                    }
                                    let _ = src.push_buffer(gst_buffer);
                                }
                            }
                        }
                    })
                    .build(),
            );
        }

        // Create PiP video recording pipeline if PiP is enabled
        let pip_pipeline = if self.show_pip {
            let pip_pipeline_str = format!(
                "appsrc name=pip_src format=time is-live=true do-timestamp=true ! \
                 videoconvert ! video/x-raw,format=I420 ! \
                 x264enc tune=zerolatency speed-preset=slower bitrate=4000 key-int-max=60 ! \
                 matroskamux ! filesink location={}",
                pip_video
            );

            println!("Using PiP pipeline: {}", pip_pipeline_str);

            let pipeline = gst::parse::launch(&pip_pipeline_str)
                .map_err(|e| anyhow::anyhow!("Failed to create PiP recording pipeline: {:?}", e))?
                .downcast::<gst::Pipeline>()
                .map_err(|_| anyhow::anyhow!("Failed to downcast to Pipeline"))?;

            if let Some(pip_src) = pipeline.by_name("pip_src") {
                let pip_src = pip_src
                    .downcast::<gstreamer_app::AppSrc>()
                    .map_err(|_| anyhow::anyhow!("Failed to downcast to AppSrc"))?;

                pip_src.set_format(gst::Format::Time);
                pip_src.set_max_bytes(1);
                pip_src.set_do_timestamp(true);

                let dims = self.pip_dimensions.lock().unwrap();
                let caps = gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", dims.width)
                    .field("height", dims.height)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build();
                pip_src.set_caps(Some(&caps));

                let frame_data = self.pip_frame_data.clone();
                let pip_src_weak = pip_src.downgrade();

                pip_src.set_callbacks(
                    gstreamer_app::AppSrcCallbacks::builder()
                        .need_data(move |_, _| {
                            if let Some(src) = pip_src_weak.upgrade() {
                                if let Ok(guard) = frame_data.lock() {
                                    if let Some(buffer) = guard.as_ref() {
                                        let mut gst_buffer = gst::Buffer::with_size(buffer.len())
                                            .expect("Failed to allocate buffer");
                                        {
                                            let buffer_mut = gst_buffer.get_mut().unwrap();
                                            let mut data = buffer_mut.map_writable().unwrap();
                                            data.copy_from_slice(buffer);
                                        }
                                        let _ = src.push_buffer(gst_buffer);
                                    }
                                }
                            }
                        })
                        .build(),
                );
            }

            Some(pipeline)
        } else {
            None
        };

        // Start recording
        main_pipeline.set_state(gst::State::Playing)?;
        if let Some(pip_pipeline) = &pip_pipeline {
            pip_pipeline.set_state(gst::State::Playing)?;
        }

        self.recording_pipeline = Some(main_pipeline);
        self.recording_files = Some((
            main_video,
            pip_video,
            format!("recording_{}.mkv", timestamp),
        ));
        self.is_recording = true;

        Ok(())
    }

    fn stop_recording(&mut self) {
        if let Some((main_video, pip_video, _)) = self.recording_files.take() {
            // Stop main recording pipeline
            if let Some(pipeline) = self.recording_pipeline.take() {
                let _ = pipeline.set_state(gst::State::Null);
                println!("Main recording saved to: {}", main_video);
            }

            // Stop PiP recording pipeline if it exists
            if let Some(pipeline) = self.pip_pipeline.take() {
                let _ = pipeline.set_state(gst::State::Null);
                println!("PiP recording saved to: {}", pip_video);
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

    fn setup_pip_webcam(&mut self) -> Result<(), anyhow::Error> {
        // Create webcam pipeline with 16:9 aspect ratio
        let pipeline_str = "avfvideosrc device-index=0 ! \
            video/x-raw,width=1280,height=720,framerate=30/1 ! \
            videoscale ! capsfilter name=size ! \
            videoconvert ! video/x-raw,format=RGBA ! \
            appsink name=pip_sink sync=false drop=true max-buffers=1";

        let pipeline = gst::parse::launch(pipeline_str)
            .map_err(|e| anyhow::anyhow!("Failed to create PiP pipeline: {:?}", e))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("Failed to downcast to Pipeline"))?;

        let appsink = pipeline
            .by_name("pip_sink")
            .ok_or(anyhow::anyhow!("Failed to find pip_sink"))?
            .downcast::<gstreamer_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("Failed to cast to AppSink"))?;

        // Configure appsink
        appsink.set_max_buffers(1);
        appsink.set_drop(true);
        appsink.set_sync(false);

        let frame_data = self.pip_frame_data.clone();
        let dimensions = self.pip_dimensions.clone();

        // Set up callbacks
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;

                    // Update dimensions from caps
                    if let Some(caps) = sample.caps() {
                        if let Some(s) = caps.structure(0) {
                            let mut dims = dimensions.lock().unwrap();
                            dims.width = s.get::<i32>("width").unwrap_or(1280);
                            dims.height = s.get::<i32>("height").unwrap_or(720);
                        }
                    }

                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                    let mut data = frame_data.lock().unwrap();
                    *data = Some(map.as_ref().to_vec());

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Set initial PiP window position and size (16:9 ratio)
        self.pip_position = egui::pos2(20.0, 20.0);
        self.pip_size = egui::vec2(320.0, 180.0); // 16:9 ratio
        self.pip_desired_size = self.pip_size;

        // Set initial caps with 16:9 aspect ratio
        if let Some(caps_filter) = pipeline.by_name("size") {
            let caps = gst::Caps::builder("video/x-raw")
                .field("width", 320i32)
                .field("height", 180i32) // Maintains 16:9
                .build();
            caps_filter.set_property("caps", &caps);
        }

        // Start the pipeline
        pipeline.set_state(gst::State::Playing)?;

        self.pip_pipeline = Some(pipeline);
        self.show_pip = true;

        Ok(())
    }

    fn update_pip_size(&mut self) {
        if let Some(pipeline) = &self.pip_pipeline {
            if let Some(caps_filter) = pipeline.by_name("size") {
                // Calculate size maintaining 16:9 aspect ratio
                let width = ((self.pip_size.x as i32 + 8) / 16) * 16;
                let height = width * 9 / 16; // Force 16:9 ratio

                // Ensure minimum size (16:9)
                let width = width.max(320);
                let height = height.max(180);

                let caps = gst::Caps::builder("video/x-raw")
                    .field("width", width)
                    .field("height", height)
                    .build();

                caps_filter.set_property("caps", &caps);
            }
        }
    }

    fn toggle_pip(&mut self) {
        if self.show_pip {
            // Stop PiP pipeline
            if let Some(pipeline) = self.pip_pipeline.take() {
                let _ = pipeline.set_state(gst::State::Null);
            }
            self.show_pip = false;
        } else {
            // Start PiP pipeline
            if let Err(e) = self.setup_pip_webcam() {
                eprintln!("Failed to start PiP webcam: {:?}", e);
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

        // Stop PiP pipeline if active
        if let Some(pipeline) = self.pip_pipeline.take() {
            if let Err(e) = pipeline.set_state(gst::State::Null) {
                eprintln!("Error stopping PiP pipeline: {:?}", e);
            }
        }

        // Stop the main pipeline
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

        // Process PiP frame data and update texture
        if self.show_pip {
            if let Ok(frame_guard) = self.pip_frame_data.lock() {
                if let Some(buffer) = frame_guard.as_ref() {
                    let dims = self.pip_dimensions.lock().unwrap();
                    let expected_size = (dims.width * dims.height * 4) as usize;

                    if buffer.len() == expected_size {
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [dims.width as usize, dims.height as usize],
                            buffer,
                        );

                        self.pip_texture = Some(ctx.load_texture(
                            "webcam-pip",
                            color_image,
                            egui::TextureOptions::default(),
                        ));
                    }
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

        // Settings button in the corner with controls
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
                    // Settings toggle
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(GEAR_ICON).font(FontId::proportional(18.0)),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        self.show_settings = !self.show_settings;
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
                                .color(if self.is_recording {
                                    egui::Color32::from_rgb(255, 80, 80)
                                } else {
                                    egui::Color32::LIGHT_GRAY
                                }),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        if self.is_recording {
                            self.stop_recording();
                        } else {
                            match self.start_recording() {
                                Ok(_) => {
                                    self.is_recording = true;
                                }
                                Err(e) => {
                                    println!("Failed to start recording: {:?}", e);
                                }
                            }
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

        // Expanded settings panel
        if self.show_settings {
            egui::Window::new("") // Empty title
                .resizable(false)
                .collapsible(false)
                .title_bar(false)
                .movable(true)
                .fixed_pos(egui::pos2(20.0, 60.0)) // Position below the controls
                .frame(
                    egui::Frame::window(&egui::Style::default())
                        .fill(egui::Color32::from_rgba_premultiplied(20, 20, 30, 250))
                        .rounding(12.0)
                        .outer_margin(0.0)
                        .inner_margin(12.0),
                )
                .show(ctx, |ui| {
                    ui.add_space(4.0);

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

                    // Handle source switching outside the UI closure
                    if let Some(idx) = selected_video_src_idx {
                        self.switch_source(idx);
                    }
                    if let Some(idx) = selected_mic_idx {
                        self.switch_mic(idx);
                    }

                    // PiP toggle in settings
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Picture in Picture")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(180, 180, 180)),
                        );
                        if ui
                            .button(if self.show_pip { "Disable" } else { "Enable" })
                            .clicked()
                        {
                            self.toggle_pip();
                        }
                    });
                });
        }

        // Show PiP window (outside of settings panel)
        if self.show_pip {
            if let Some(texture) = &self.pip_texture {
                let mut new_position = self.pip_position;
                let mut should_update_size = false;

                egui::Window::new("Webcam")
                    .title_bar(false)
                    .resizable(true)
                    .default_size(self.pip_desired_size)
                    .min_size(egui::vec2(160.0, 120.0))
                    .fixed_pos(self.pip_position)
                    .frame(
                        egui::Frame::none()
                            .inner_margin(8.0)
                            .rounding(8.0)
                            .fill(egui::Color32::from_rgba_premultiplied(20, 20, 30, 200)),
                    )
                    .show(ctx, |ui| {
                        let available_size = ui.available_size();
                        let dims = self.pip_dimensions.lock().unwrap();
                        let aspect_ratio = dims.width as f32 / dims.height as f32;

                        // Calculate display size maintaining aspect ratio
                        let mut display_size = available_size;
                        let current_ratio = display_size.x / display_size.y;

                        if current_ratio > aspect_ratio {
                            display_size.x = display_size.y * aspect_ratio;
                        } else {
                            display_size.y = display_size.x / aspect_ratio;
                        }

                        // Ensure minimum size
                        display_size.x = display_size.x.max(160.0);
                        display_size.y = display_size.y.max(120.0);

                        let response = ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(display_size)
                                .sense(egui::Sense::drag())
                                .rounding(4.0),
                        );

                        if response.dragged() {
                            new_position += response.drag_delta();
                        }

                        // Store actual display size
                        self.pip_size = display_size;

                        // Check if we should update pipeline size
                        if response.drag_stopped() {
                            should_update_size = true;
                        }
                    });

                // Apply position update
                self.pip_position = new_position;

                // Update pipeline size if needed
                if should_update_size {
                    self.update_pip_size();
                }
            }
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
