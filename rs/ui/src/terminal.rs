use crate::model::UiModel;
use crate::render::render;

const CLEAR_HOME: &str = "\x1b[2J\x1b[H";
const FALLBACK: (usize, usize) = (80, 24);

pub struct Frame;

impl Frame {
    pub fn size() -> (usize, usize) {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            let ok = libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws as *mut _);
            if ok == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
                (ws.ws_col as usize, ws.ws_row as usize)
            } else {
                FALLBACK
            }
        }
    }

    pub fn draw(model: &UiModel) -> String {
        let (cols, rows) = Self::size();
        Self::draw_at(model, cols, rows)
    }

    pub fn draw_at(model: &UiModel, cols: usize, rows: usize) -> String {
        let mut out = String::with_capacity(cols * rows + CLEAR_HOME.len());
        out.push_str(CLEAR_HOME);
        out.push_str(&render(model, cols, rows));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_never_returns_zero() {
        let (cols, rows) = Frame::size();
        assert!(cols > 0);
        assert!(rows > 0);
    }

    #[test]
    fn draw_at_starts_with_clear_and_home() {
        let model = UiModel::new();
        let frame = Frame::draw_at(&model, 60, 20);
        assert!(frame.starts_with(CLEAR_HOME));
        assert_eq!(frame[CLEAR_HOME.len()..].split('\n').count(), 20);
    }
}
