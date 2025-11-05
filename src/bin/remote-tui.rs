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

use std::time::Duration;
use std::{os::unix::net::UnixStream, thread};

use anyhow::{anyhow, Context, Result};
use nusb::{list_devices, DeviceInfo, MaybeFuture};
use usbvfiod::hotplug_protocol::command::Command;
use usbvfiod::hotplug_protocol::response::Response;

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

const SOCKET_PATH: &str = "/tmp/usbvfiod-hot-attach";

fn main() -> io::Result<()> {
    let mut terminal = init()?;
    let mut app = App::new();

    let sdd = app.device_data.clone();
    thread::spawn(move || update_device_list(sdd));

    let at = app.attached.clone();
    thread::spawn(move || update_attached_list(at));

    let app_result = app.run(&mut terminal);
    restore()?;
    app_result
}

#[derive(Debug)]
pub struct App {
    device_data: Arc<Mutex<Vec<DeviceInfo>>>,
    attached: Arc<Mutex<Vec<(u8, u8)>>>,
    selected_index: usize,
    exit: bool,
}

impl App {
    fn new() -> Self {
        App {
            device_data: Arc::new(Mutex::new(vec![])),
            attached: Arc::new(Mutex::new(vec![])),
            selected_index: 0,
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
        render_device_list(self, horizontal_split[0], frame.buffer_mut());
        render_device_info(self, horizontal_split[1], frame.buffer_mut());
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
            KeyCode::Esc => self.detach(),
            _ => {}
        };
    }

    fn exit(&mut self) {
        self.exit = true;
    }

    fn up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    fn down(&mut self) {
        if self.selected_index < self.device_data.lock().unwrap().len() - 1 {
            self.selected_index += 1;
        }
    }

    fn attach(&mut self) {
        if let Some(device_info) = self.device_data.lock().unwrap().get(self.selected_index) {
            let bus_num = device_info.busnum();
            let dev_num = device_info.device_address();
            let _ = attach_usbvfiod(bus_num, dev_num);
        }
    }

    fn detach(&mut self) {
        if let Some(device_info) = self.device_data.lock().unwrap().get(self.selected_index) {
            let bus_num = device_info.busnum();
            let dev_num = device_info.device_address();
            let _ = detach_usbvfiod(bus_num, dev_num);
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

fn render_device_list(app: &App, area: Rect, buf: &mut Buffer) {
    let device_data: Vec<DeviceInfo> = app.device_data.lock().unwrap().to_vec();
    let attached = app.attached.lock().unwrap().to_vec();

    let title = Line::from(" Device Selector ".bold());
    let mut instruction_parts = vec![
        " Up".into(),
        " <\u{2191}> ".blue().bold(),
        " Down".into(),
        " <\u{2193}> ".blue().bold(),
    ];
    if let Some(dev) = device_data.get(app.selected_index) {
        let is_selected_device_attached = attached.contains(&(dev.busnum(), dev.device_address()));
        if is_selected_device_attached {
            instruction_parts.append(&mut vec![" Detach".into(), " <Esc> ".blue().bold()]);
        } else {
            instruction_parts.append(&mut vec![" Attach".into(), " <Space> ".blue().bold()]);
        }
    }
    let instructions = Line::from(instruction_parts);
    let block = Block::default()
        .title_top(title.centered())
        .title_bottom(instructions.centered())
        .borders(Borders::ALL)
        .border_set(border::THICK);

    let device_names = device_data;
    let counter_text = Text::from(
        device_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let is_attached = attached.contains(&(name.busnum(), name.device_address()));
                Line::from(vec![
                    if i == app.selected_index {
                        "\u{27A1} ".yellow().bold()
                    } else {
                        "  ".into()
                    },
                    {
                        let text: String = format!(
                            "{:03}:{:03} {} {}",
                            name.busnum(),
                            name.device_address(),
                            name.manufacturer_string().unwrap(),
                            name.product_string().unwrap(),
                        )
                        .into();
                        if is_attached {
                            text.yellow().bold()
                        } else {
                            text.into()
                        }
                    },
                    if is_attached { " (attached)" } else { "" }.bold(),
                ])
            })
            .collect::<Vec<_>>(),
    );

    Paragraph::new(counter_text).block(block).render(area, buf);
}

fn render_device_info(app: &App, area: Rect, buf: &mut Buffer) {
    let title = Line::from(" Device Info ".bold());
    let block = Block::default()
        .title(title.centered())
        .borders(Borders::ALL)
        .border_set(border::THICK);

    let counter_text = match app
        .device_data
        .lock()
        .unwrap()
        .get(app.selected_index)
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
    let device_path = format!("/dev/bus/usb/{:03}/{:03}", bus_number, device_number);

    let open_file = |err_msg: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device_path)
            .with_context(|| err_msg.to_string())
    };

    let file = open_file("Failed to open USB device file")?;
    let device = nusb::Device::from_fd(file.into())
        .wait()
        .context("Failed to open nusb device")?;
    device.reset().wait().context("Failed to reset device")?;

    // After the reset, the device instance is no longer usable and we need
    // to reopen.
    let file = open_file("Failed to open USB device file after device reset")?;

    // write to socket for hot-attach fds
    let command = Command::Attach {
        bus: bus_number,
        device: device_number,
        fd: file,
    };
    let mut socket = UnixStream::connect(SOCKET_PATH).context("Failed to open socket")?;
    command
        .send_over_socket(&socket)
        .context("Failed to send attach command over the socket")?;

    Response::receive_from_socket(&mut socket)
        .context("Failed to receive response over the socket")?;

    Ok(())
}

fn detach_usbvfiod(bus: u8, dev: u8) -> Result<()> {
    let command = Command::Detach { bus, device: dev };
    let mut socket = UnixStream::connect(SOCKET_PATH).context("Failed to open socket")?;
    command
        .send_over_socket(&socket)
        .context("Failed to send detach command over the socket")?;

    Response::receive_from_socket(&mut socket)
        .context("Failed to receive response over the socket")?;

    Ok(())
}

fn update_device_list(device_list: Arc<Mutex<Vec<DeviceInfo>>>) {
    loop {
        let mut devices = list_devices().wait().unwrap().collect::<Vec<DeviceInfo>>();
        devices.sort_by_key(|dev| (dev.busnum(), dev.device_address()));
        *device_list.lock().unwrap() = devices;
        thread::sleep(Duration::from_millis(500));
    }
}

fn update_attached_list(attached_list: Arc<Mutex<Vec<(u8, u8)>>>) {
    loop {
        if let Ok(attached) = list_attached() {
            *attached_list.lock().unwrap() = attached;
        }
        thread::sleep(Duration::from_millis(500));
    }
}
fn list_attached() -> Result<Vec<(u8, u8)>> {
    let mut socket = UnixStream::connect(SOCKET_PATH).context("Failed to open socket")?;
    Command::List
        .send_over_socket(&socket)
        .context("Failed to send list command over socket")?;

    let response = Response::receive_from_socket(&mut socket)
        .context("Failed to receive response over the socket")?;

    if response != Response::ListFollowing {
        return Err(anyhow!(
            "Expected the response {:?} but got {:?}",
            Response::ListFollowing,
            response
        ));
    }

    let device_list = response.receive_devices_list(&mut socket)?;
    Ok(device_list)
}
