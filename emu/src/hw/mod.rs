extern crate byteorder;
extern crate gl;
extern crate sdl2;

mod glutils;

use self::glutils::SurfaceRenderer;
use self::sdl2::event::Event;
use self::sdl2::keyboard::Keycode;
use self::sdl2::video::{GLContext, GLProfile, Window};
use self::sdl2::VideoSubsystem;
use super::dbg::{Debugger, DebuggerModel};
use super::gfx::{GfxBufferLE, GfxBufferMutLE, OwnedGfxBufferLE, Rgb888};
use std::ops::DerefMut;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

pub struct OutputConfig {
    pub window_title: String,
    pub width: isize,
    pub height: isize,
    pub fps: isize,
    pub enforce_speed: bool,
}

struct VideoDebugger {
    window: Window,
    gl_context: GLContext,
}

struct Video {
    video: VideoSubsystem,
    window: Window,
    // canvas: WindowCanvas,
    // creator: TextureCreator<WindowContext>,
    renderer: SurfaceRenderer,
    dbg: Option<VideoDebugger>,
    gl_context: GLContext,

    cfg: Rc<OutputConfig>,
    fps_clock: SystemTime,
    fps_counter: isize,
}

impl Video {
    fn new(cfg: Rc<OutputConfig>, context: &sdl2::Sdl) -> Result<Video, String> {
        let video = context
            .video()
            .or_else(|e| Err(format!("error creating video subsystem: {:?}", e)))?;

        // Request OpenGL Core profile (for GL 3.2 extensions, required by imgui-opengl-renderer).
        {
            let gl_attr = video.gl_attr();
            gl_attr.set_context_profile(GLProfile::Core);
            gl_attr.set_context_version(3, 0);
        }

        let window = video
            .window(&cfg.window_title, 800, 600)
            .resizable()
            .position_centered()
            .opengl()
            .allow_highdpi()
            .build()
            .or_else(|e| Err(format!("error creating window: {:?}", e)))?;

        let gl_context = window
            .gl_create_context()
            .expect("couldn't create GL context");

        let video2 = video.clone();
        let renderer = SurfaceRenderer::new(move |s| video2.gl_get_proc_address(s) as _);

        Ok(Video {
            cfg,
            video,
            window,
            renderer,
            gl_context,
            dbg: None,
            fps_clock: SystemTime::now(),
            fps_counter: 0,
        })
    }

    fn render_frame(&mut self, frame: &GfxBufferLE<Rgb888>) {
        self.renderer.render(frame);
        self.update_fps();
    }

    fn update_fps(&mut self) {
        self.fps_counter += 1;
        let one_second = Duration::new(1, 0);
        match self.fps_clock.elapsed() {
            Ok(elapsed) if elapsed >= one_second => {
                self.window.set_title(&format!(
                    "{} - {} FPS",
                    &self.cfg.window_title, self.fps_counter
                ));
                self.fps_counter = 0;
                self.fps_clock += one_second;
            }
            _ => {}
        }
    }
}

pub trait OutputProducer {
    fn render_frame(&mut self, screen: &mut GfxBufferMutLE<Rgb888>);
    fn finish(&mut self);
}

pub struct Output {
    cfg: Rc<OutputConfig>,
    context: sdl2::Sdl,
    video: Option<Video>,
}

impl Output {
    pub fn new(cfg: OutputConfig) -> Result<Output, String> {
        Ok(Output {
            cfg: Rc::new(cfg),
            context: sdl2::init()?,
            video: None,
        })
    }

    pub fn enable_video(&mut self) -> Result<(), String> {
        self.video = Some(Video::new(self.cfg.clone(), &self.context)?);
        Ok(())
    }

    pub fn run_and_debug<P: OutputProducer + DebuggerModel>(&mut self, producer: &mut P) {
        let width = self.cfg.width as usize;
        let height = self.cfg.height as usize;
        let mut debugger = match self.video {
            Some(ref v) => Some(Debugger::new(v.video.clone())),
            None => None,
        };

        let mut event_pump = self.context.event_pump().unwrap();
        loop {
            for event in event_pump.poll_iter() {
                debugger.as_mut().map(|dbg| {
                    dbg.handle_event(&event);
                });

                match event {
                    Event::KeyDown {
                        keycode: Some(Keycode::Escape),
                        ..
                    }
                    | Event::Quit { .. } => return,
                    _ => {}
                }
            }

            let mut screen = OwnedGfxBufferLE::<Rgb888>::new(width, height);
            producer.render_frame(&mut screen.buf_mut());

            if let Some(v) = self.video.as_mut() {
                v.render_frame(&screen.buf());

                debugger.as_mut().map(|dbg| {
                    dbg.render_frame(&v.window, &event_pump, producer);
                });

                v.window.gl_swap_window();
            }
        }
    }

    /// Run a blocking loop in which output is produced by a OutputProducer,
    /// until the producer exits by itself, or the user closes the window.
    /// The OutputProducer is run in a background thread, so to parallelize
    /// display visualization and vsync with actual output generation.
    ///
    /// create is a FnOnce callback that creates a OutputProducer, and is invoked
    /// in the background thread so that OutputProducer needs not to implement
    /// Send.
    pub fn run_threaded<F: 'static + Send + FnOnce() -> Result<Box<OutputProducer>, String>>(
        &mut self,
        create: F,
    ) {
        let width = self.cfg.width as usize;
        let height = self.cfg.height as usize;
        let (tx, rx) = mpsc::sync_channel(3);

        thread::spawn(move || {
            let mut producer = create().unwrap();
            loop {
                let mut screen = OwnedGfxBufferLE::<Rgb888>::new(width, height);
                producer.render_frame(&mut screen.buf_mut());

                tx.send(screen).unwrap();
            }
        });

        let polling_interval = Duration::from_millis(5);
        loop {
            for event in self.context.event_pump().unwrap().poll_iter() {
                match event {
                    Event::KeyDown {
                        keycode: Some(Keycode::Escape),
                        ..
                    }
                    | Event::Quit { .. } => return,
                    _ => {}
                }
            }

            match rx.recv_timeout(polling_interval) {
                Ok(ref screen) => self.render_frame(&screen.buf()),
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Render a single frame to the video output.
    pub fn render_frame(&mut self, screen: &GfxBufferLE<Rgb888>) {
        if let Some(v) = self.video.as_mut() {
            v.render_frame(&screen);
            v.window.gl_swap_window();
        }
    }
}
