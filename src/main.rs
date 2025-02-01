use std::time::Instant;
use windows_capture::{
    capture::GraphicsCaptureApiHandler,
    frame::{Frame, ImageFormat},
    graphics_capture_api::InternalCaptureControl,
    settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings},
    window::Window,
};

struct ScreenshotCapture {
    start_time: Instant,
    screenshot_path: String,
    captured: bool,
}

impl GraphicsCaptureApiHandler for ScreenshotCapture {
    type Flags = String;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(_: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            start_time: Instant::now(),
            screenshot_path: "tarkov_screenshot.png".to_string(),
            captured: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if !self.captured {
            frame.save_as_image(&self.screenshot_path, ImageFormat::Png)?;
            let duration = self.start_time.elapsed();
            println!("Screenshot captured in: {:.2?}", duration);
            self.captured = true;
            capture_control.stop();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Capture session ended");
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target_window = Window::from_name("EscapeFromTarkov")
        .map_err(|_| "Failed to find EscapeFromTarkov window")?;

    let settings = Settings::new(
        target_window,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        ColorFormat::Rgba8,
        String::new(),
    );

    ScreenshotCapture::start(settings)?;
    Ok(())
}
