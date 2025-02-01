use std::time::Instant;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use windows_capture::{
    capture::GraphicsCaptureApiHandler,
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings},
    window::Window,
};
use std::fs::File;
use std::io::BufWriter;

#[derive(Clone)]
struct FrameData {
    pixels: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    stride: u32,
    timestamp: Instant,
}

struct FrameRecorder {
    buffer: Arc<Mutex<VecDeque<FrameData>>>,
    max_frames: usize,
    tx: tokio::sync::mpsc::Sender<FrameData>,
}

impl FrameRecorder {
    pub fn new(max_frames: usize) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let buffer = Arc::new(Mutex::new(VecDeque::with_capacity(max_frames)));
        
        let buffer_clone = buffer.clone();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let mut guard = buffer_clone.lock().await;
                if guard.len() >= max_frames {
                    guard.pop_front();
                }
                guard.push_back(frame);
            }
        });
        
        Self { buffer, max_frames, tx }
    }

    pub async fn get_latest(&self) -> Option<FrameData> {
        let guard = self.buffer.lock().await;
        guard.back().cloned()
    }

    pub fn get_sender(&self) -> tokio::sync::mpsc::Sender<FrameData> {
        self.tx.clone()
    }
}

struct CaptureHandler {
    tx: tokio::sync::mpsc::Sender<FrameData>,
    start_time: Instant,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = String;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(_: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            start_time: Instant::now(),
            tx: GLOBAL_SENDER.lock().unwrap().clone(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();
        let stride = width * 4;  // RGBA8 format = 4 bytes per pixel
        
        // Get the raw pixel data from the frame
        if let Ok(mut frame_buffer) = frame.buffer() {
            let buffer_size = (height * stride) as usize;
            let pixels = frame_buffer.as_nopadding_buffer()
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .to_vec();
            
            // Create frame data with the buffer size
            let frame_data = FrameData {
                pixels: Arc::new(pixels),
                width,
                height,
                stride,
                timestamp: Instant::now(),
            };
            
            self.tx.blocking_send(frame_data).unwrap_or_else(|e| println!("Failed to send frame data: {}", e));
        } else {
            println!("Failed to get frame buffer");
        }
        
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Capture session ended");
        Ok(())
    }
}

// Global sender for the capture handler
lazy_static::lazy_static! {
    static ref GLOBAL_SENDER: std::sync::Mutex<tokio::sync::mpsc::Sender<FrameData>> = {
        let (tx, _) = tokio::sync::mpsc::channel(32);
        std::sync::Mutex::new(tx)
    };
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting capture...");
    let recorder = FrameRecorder::new(4); // Keep 60 frames in the buffer
    
    // Store the sender globally for the capture handler
    *GLOBAL_SENDER.lock().unwrap() = recorder.get_sender();

    let target_window = Window::from_name("EscapeFromTarkov")
        .map_err(|_| "Failed to find EscapeFromTarkov window")?;
    println!("Found target window");

    let settings = Settings::new(
        target_window,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        ColorFormat::Rgba8,
        String::new(),
    );

    // Start capture in a blocking thread
    println!("Starting capture thread...");
    let capture_handle = tokio::task::spawn_blocking(move || {
        println!("Capture thread started");
        CaptureHandler::start(settings)
    });

    // Example: Get the latest frame every second
    println!("Entering main loop...");
    loop {
        if let Some(frame_data) = recorder.get_latest().await {
            println!(
                "Got frame: {}x{}, {} bytes", 
                frame_data.width, 
                frame_data.height, 
                frame_data.pixels.len()
            );
            // Save frame as PNG
            let filename = format!("frame_{}.png", frame_data.timestamp.elapsed().as_millis());
            let file = File::create(&filename).unwrap();
            let w = BufWriter::new(file);
            let mut encoder = png::Encoder::new(w, frame_data.width, frame_data.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&frame_data.pixels).unwrap();
            println!("Saved frame to {}", filename);
        } else {
            println!("No frame available");
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}
