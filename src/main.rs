use eframe::egui;
use egui::ViewportBuilder;
use gstreamer as gst;
use gstreamer::prelude::Cast;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::mpsc;

struct ScreenCapApp {
    texture: Option<egui::TextureHandle>,
    receiver: mpsc::Receiver<Vec<u8>>,
    width: i32,
    height: i32,
    is_recording: bool,
    pipeline: gst::Pipeline,
}

impl ScreenCapApp {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        receiver: mpsc::Receiver<Vec<u8>>,
        width: i32,
        height: i32,
        pipeline: gst::Pipeline,
    ) -> Self {
        Self {
            texture: None,
            receiver,
            width,
            height,
            is_recording: false,
            pipeline,
        }
    }
}

impl eframe::App for ScreenCapApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Ok(buffer) = self.receiver.try_recv() {
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

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                if let Some(texture) = &self.texture {
                    ui.image(texture);
                }

                let button_text = if self.is_recording {
                    "Stop Recording"
                } else {
                    "Start Recording"
                };

                if ui.button(button_text).clicked() {
                    self.is_recording = !self.is_recording;
                    if self.is_recording {
                        if let Some(tee) = self.pipeline.by_name("t") {
                            tee.set_property("allow-not-linked", true);
                        }
                        println!("Started recording");
                    } else {
                        if let Some(tee) = self.pipeline.by_name("t") {
                            tee.set_property("allow-not-linked", false);
                        }
                        println!("Stopped recording");
                    }
                }
            });
        });

        ctx.request_repaint();
    }
}

fn setup_gstreamer() -> (mpsc::Receiver<Vec<u8>>, i32, i32, gst::Pipeline) {
    gst::init().unwrap();

    // Smaller default dimensions for better usability
    let default_width = 960; // Half of 1920
    let default_height = 540; // Half of 1080

    // Create source pipeline based on OS with scaling
    let source_element = match std::env::consts::OS {
        "macos" => "avfvideosrc capture-screen=true",
        "windows" => "d3d11screencapturesrc",
        "linux" => "ximagesrc",
        _ => panic!("Unsupported operating system"),
    };

    // Build pipeline with videoscale and tee
    let pipeline_str = format!(
        "{} ! videoscale ! video/x-raw,width={},height={},framerate=30/1 ! tee name=t \
         t. ! queue ! videoconvert ! video/x-raw,format=RGBA ! appsink name=sink \
         t. ! queue ! videoconvert ! x264enc tune=zerolatency ! mp4mux ! filesink name=filesink location=recording.mp4",
        source_element, default_width, default_height
    );

    let pipeline = gst::parse::launch(&pipeline_str).unwrap();
    let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();

    // Set up appsink for display
    let (sender, receiver) = mpsc::channel();
    let appsink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                sender.send(map.to_vec()).unwrap();
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    // Initially disable the recording branch
    if let Some(tee) = pipeline.by_name("t") {
        tee.set_property("allow-not-linked", false);
    }

    pipeline.set_state(gst::State::Playing).unwrap();

    (receiver, default_width, default_height, pipeline)
}

fn main() -> Result<(), eframe::Error> {
    let (receiver, width, height, pipeline) = setup_gstreamer();

    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default(),
        ..Default::default()
    };

    eframe::run_native(
        "Screen Capture",
        options,
        Box::new(|cc| {
            Ok(Box::new(ScreenCapApp::new(
                cc, receiver, width, height, pipeline,
            )))
        }),
    )
}
