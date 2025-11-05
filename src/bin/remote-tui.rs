#![deny(
    clippy::all,
    clippy::cargo,
    clippy::nursery,
    clippy::must_use_candidate
)]
// now allow a few rules which are denied by the above's statement
#![allow(clippy::multiple_crate_versions)]
#![deny(missing_debug_implementations)]
#![deny(rustdoc::all)]

//! usbvfiod

use std::os::unix::net::UnixStream;
use std::{os::fd::AsRawFd, time::Duration};

use anyhow::{Context, Result};
use nusb::{list_devices, DeviceInfo, MaybeFuture};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

use std::{
    io,
    sync::{Arc, Mutex},
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    prelude::*,
    symbols::border,
    widgets::{block::*, *},
};
use std::io::{stdout, Stdout};

use crossterm::{execute, terminal::*};

/// A type alias for the terminal type used in this application
type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Initialize the terminal
fn init() -> io::Result<Tui> {
    execute!(stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    Terminal::new(CrosstermBackend::new(stdout()))
}

/// Restore the terminal to its original state
fn restore() -> io::Result<()> {
    execute!(stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

fn main() -> io::Result<()> {
    let devices = list_devices().wait().unwrap().collect();
    let shared_device_data = Arc::new(Mutex::new(devices));
    let mut terminal = init()?;
    let app_result = App::new(shared_device_data).run(&mut terminal);
    restore()?;
    app_result
}

#[derive(Debug)]
pub struct App {
    device_data: Arc<Mutex<Vec<DeviceInfo>>>,
    attached: Arc<Mutex<Vec<(u8, u8)>>>,
    selected_vm_index: usize,
    exit: bool,
}

impl App {
    fn new(device_data: Arc<Mutex<Vec<DeviceInfo>>>) -> Self {
        App {
            device_data,
            attached: Arc::new(Mutex::new(vec![])),
            selected_vm_index: 0,
            exit: false,
        }
    }
}

impl App {
    /// runs the application's main loop until the user quits
    pub fn run(&mut self, terminal: &mut Tui) -> io::Result<()> {
        while !self.exit {
            terminal.draw(|frame| self.render_frame(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn render_frame(&self, frame: &mut Frame) {
        let vert_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Fill(1)])
            .split(frame.area());

        let horizontal_split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(vert_split[1]);

        render_header(self, vert_split[0], frame.buffer_mut());
        render_vm_list(self, horizontal_split[0], frame.buffer_mut());
        render_vm_info(self, horizontal_split[1], frame.buffer_mut());
    }

    /// updates the application's state based on user input
    fn handle_events(&mut self) -> io::Result<()> {
        if crossterm::event::poll(Duration::from_millis(50))? {
            match event::read()? {
                // it's important to check that the event is a key press event as
                // crossterm also emits key release and repeat events on Windows.
                Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                    self.handle_key_event(key_event)
                }
                _ => {}
            };
        }
        Ok(())
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Char('q') => self.exit(),
            KeyCode::Up => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Char(' ') => self.attach(),
            _ => {}
        };
    }

    fn exit(&mut self) {
        self.exit = true;
    }

    fn up(&mut self) {
        if self.selected_vm_index > 0 {
            self.selected_vm_index -= 1;
        }
    }

    fn down(&mut self) {
        if self.selected_vm_index < self.device_data.lock().unwrap().len() - 1 {
            self.selected_vm_index += 1;
        }
    }

    fn attach(&mut self) {
        let device_info = &self.device_data.lock().unwrap()[self.selected_vm_index];
        let bus_num = device_info.busnum();
        let dev_num = device_info.device_address();
        if let Ok(_) = attach_usbvfiod(bus_num, dev_num) {
            self.attached.lock().unwrap().push((bus_num, dev_num));
        }
    }
}

fn render_header(_app: &App, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::THICK);

    let counter_text =
        vec![Line::from(vec!["usbvfiod".bold().yellow(), " Control Center".into()]).centered()];

    Paragraph::new(counter_text).block(block).render(area, buf);
}

fn render_vm_list(app: &App, area: Rect, buf: &mut Buffer) {
    let title = Line::from(" Device Selector ".bold());
    let instructions = Line::from(Line::from(vec![
        " Up".into(),
        " <\u{2191}> ".blue().bold(),
        " Down".into(),
        " <\u{2193}> ".blue().bold(),
        " Connect".into(),
        "<\u{2192}> ".blue().bold(),
    ]));
    let block = Block::default()
        .title_top(title.centered())
        .title_bottom(instructions.centered())
        .borders(Borders::ALL)
        .border_set(border::THICK);

    let device_data: Vec<DeviceInfo> = app.device_data.lock().unwrap().to_vec();
    let attached = app.attached.lock().unwrap().to_vec();
    let vm_names = device_data;
    let counter_text = Text::from(
        vm_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                Line::from(vec![
                    if i == app.selected_vm_index {
                        "\u{27A1} ".yellow().bold()
                    } else {
                        "  ".into()
                    },
                    format!(
                        "{:03}:{:03} {} {}",
                        name.busnum(),
                        name.device_address(),
                        name.manufacturer_string().unwrap(),
                        name.product_string().unwrap(),
                    )
                    .into(),
                    if attached.contains(&(name.busnum(), name.device_address())) {
                        " (attached)"
                    } else {
                        ""
                    }
                    .bold(),
                ])
            })
            .collect::<Vec<_>>(),
    );

    Paragraph::new(counter_text).block(block).render(area, buf);
}

fn render_vm_info(app: &App, area: Rect, buf: &mut Buffer) {
    let title = Line::from(" Device Info ".bold());
    let block = Block::default()
        .title(title.centered())
        .borders(Borders::ALL)
        .border_set(border::THICK);

    let counter_text = match app
        .device_data
        .lock()
        .unwrap()
        .get(app.selected_vm_index)
        .clone()
    {
        None => Text::from(vec![]),
        Some(device_data_selected) => Text::from(vec![
            Line::from(vec![
                "Bus number: ".into(),
                device_data_selected.busnum().to_string().clone().yellow(),
            ]),
            Line::from(vec![
                "Device number: ".into(),
                device_data_selected
                    .device_address()
                    .to_string()
                    .clone()
                    .yellow(),
            ]),
            Line::from(vec![
                "Vendor id: ".into(),
                format!("{:x}", device_data_selected.vendor_id()).yellow(),
            ]),
            Line::from(vec![
                "Product id: ".into(),
                format!("{:x}", device_data_selected.product_id()).yellow(),
            ]),
            Line::from(vec![
                "Manufacturer string: ".into(),
                device_data_selected
                    .manufacturer_string()
                    .unwrap()
                    .to_string()
                    .yellow(),
            ]),
            Line::from(vec![
                "Product string: ".into(),
                device_data_selected
                    .product_string()
                    .unwrap()
                    .to_string()
                    .yellow(),
            ]),
            Line::from(vec![
                "USB version: ".into(),
                format!(
                    "{}.{}",
                    device_data_selected.usb_version() >> 8,
                    device_data_selected.usb_version() & 0xff >> 4
                )
                .yellow(),
            ]),
            Line::from(vec![
                "Speed: ".into(),
                format!("{:?} Speed", device_data_selected.speed().unwrap()).yellow(),
            ]),
        ]),
    };

    Paragraph::new(counter_text).block(block).render(area, buf);
}

fn attach_usbvfiod(bus_number: u8, device_number: u8) -> Result<()> {
    let path = format!("/dev/bus/usb/{:03}/{:03}", bus_number, device_number);
    let open_file = |err_msg| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("{}: {}", err_msg, path))
    };

    let file = open_file("Failed to open USB device file")?;
    let device = nusb::Device::from_fd(file.into()).wait()?;
    device.reset().wait()?;

    // After the reset, the device instance is no longer usable and we need
    // to reopen.
    let file = open_file("Failed to open USB device file after device reset")?;

    // write to socket for hot-attach fds
    let socket = UnixStream::connect("/tmp/usbvfiod-hot-attach").unwrap();
    let buf = [0u8; 1];
    let _byte_count = socket.send_with_fd(&buf[..], file.as_raw_fd()).unwrap();

    Ok(())
}
