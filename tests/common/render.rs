use mach::app::App;
use mach::input::handle_event;
use mach::ui;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};

pub fn draw(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| ui::draw(frame, app))
        .expect("draw must not panic");
    terminal.backend().buffer().clone()
}

pub fn buffer_text(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn find_cells(buffer: &Buffer, needle: &str) -> (u16, u16) {
    let width = needle.chars().count() as u16;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width.saturating_sub(width) {
            let run: String = (x..x + width).map(|x| buffer[(x, y)].symbol()).collect();
            if run == needle {
                return (x, y);
            }
        }
    }
    panic!("{needle:?} is not on screen:\n{}", buffer_text(buffer));
}

pub fn move_mouse(app: &mut App, column: u16, row: u16) -> bool {
    handle_event(
        app,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    )
}
