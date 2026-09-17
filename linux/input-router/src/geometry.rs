//! Logical display geometry: adjacent Ubuntu and Windows screens,
//! edge-crossing detection, and canonical coordinate mapping.
//!
//! Pure model, fully unit-tested; no OS dependencies. The capture layer
//! (portal or evdev) feeds it motion; it answers "where is the logical
//! cursor and did we cross a boundary?".

pub const CANONICAL_MAX: f64 = 65535.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Local,
    Remote,
}

/// Which local edge borders the remote screen. Only Right/Left for now;
/// the model generalizes later if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Right,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crossing {
    None,
    ToRemote,
    ToLocal,
}

#[derive(Debug)]
pub struct Space {
    pub local: Screen,
    pub remote: Screen,
    pub edge: Edge,
    pub target: Target,
    /// Cursor position in the CURRENT target's pixel space.
    pub x: f64,
    pub y: f64,
}

impl Space {
    pub fn new(local: Screen, remote: Screen, edge: Edge) -> Self {
        Space {
            local,
            remote,
            edge,
            target: Target::Local,
            x: local.width / 2.0,
            y: local.height / 2.0,
        }
    }

    fn cur_screen(&self) -> Screen {
        match self.target {
            Target::Local => self.local,
            Target::Remote => self.remote,
        }
    }

    /// Normalized vertical mapping between screens of different heights.
    fn map_y(from: Screen, to: Screen, y: f64) -> f64 {
        (y / from.height * to.height).clamp(0.0, to.height - 1.0)
    }

    /// Apply a relative motion delta; returns whether a boundary was crossed.
    /// On a crossing, position is re-seated in the new target's space with
    /// vertical continuity (normalized mapping).
    pub fn apply_delta(&mut self, dx: f64, dy: f64) -> Crossing {
        let s = self.cur_screen();
        let nx = self.x + dx;
        self.y = (self.y + dy).clamp(0.0, s.height - 1.0);

        let crossing = match (self.target, self.edge) {
            // Local -> remote across the shared edge
            (Target::Local, Edge::Right) if nx > s.width - 1.0 => Crossing::ToRemote,
            (Target::Local, Edge::Left) if nx < 0.0 => Crossing::ToRemote,
            // Remote -> local across the same shared edge (opposite side)
            (Target::Remote, Edge::Right) if nx < 0.0 => Crossing::ToLocal,
            (Target::Remote, Edge::Left) if nx > s.width - 1.0 => Crossing::ToLocal,
            _ => Crossing::None,
        };

        match crossing {
            Crossing::None => {
                self.x = nx.clamp(0.0, s.width - 1.0);
            }
            Crossing::ToRemote => {
                self.y = Self::map_y(self.local, self.remote, self.y);
                self.x = match self.edge {
                    Edge::Right => 0.0,
                    Edge::Left => self.remote.width - 1.0,
                };
                self.target = Target::Remote;
            }
            Crossing::ToLocal => {
                self.y = Self::map_y(self.remote, self.local, self.y);
                self.x = match self.edge {
                    Edge::Right => self.local.width - 1.0,
                    Edge::Left => 0.0,
                };
                self.target = Target::Local;
            }
        }
        crossing
    }

    /// Current remote position in canonical 0..65535 coordinates.
    /// Only meaningful while target == Remote.
    pub fn canonical(&self) -> (u16, u16) {
        let cx = (self.x / (self.remote.width - 1.0) * CANONICAL_MAX)
            .clamp(0.0, CANONICAL_MAX);
        let cy = (self.y / (self.remote.height - 1.0) * CANONICAL_MAX)
            .clamp(0.0, CANONICAL_MAX);
        (cx.round() as u16, cy.round() as u16)
    }

    /// Enter the remote screen through the shared edge at the given local
    /// vertical position (used when the compositor reports an activation).
    pub fn enter_remote(&mut self, local_y: f64) {
        self.y = Self::map_y(
            self.local,
            self.remote,
            local_y.clamp(0.0, self.local.height - 1.0),
        );
        self.x = match self.edge {
            Edge::Right => 0.0,
            Edge::Left => self.remote.width - 1.0,
        };
        self.target = Target::Remote;
    }

    /// Force the cursor back to the local screen (emergency return).
    /// Re-seats at the shared edge with vertical continuity.
    pub fn force_local(&mut self) {
        if self.target == Target::Remote {
            self.y = Self::map_y(self.remote, self.local, self.y);
            self.x = match self.edge {
                Edge::Right => self.local.width - 1.0,
                Edge::Left => 0.0,
            };
            self.target = Target::Local;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> Space {
        // Ubuntu 2560x1440, Windows 1920x1080, Windows to the right.
        Space::new(
            Screen { width: 2560.0, height: 1440.0 },
            Screen { width: 1920.0, height: 1080.0 },
            Edge::Right,
        )
    }

    #[test]
    fn starts_local_centered() {
        let s = space();
        assert_eq!(s.target, Target::Local);
        assert_eq!((s.x, s.y), (1280.0, 720.0));
    }

    #[test]
    fn motion_clamps_inside_screen() {
        let mut s = space();
        assert_eq!(s.apply_delta(-99999.0, -99999.0), Crossing::None);
        assert_eq!((s.x, s.y), (0.0, 0.0));
        assert_eq!(s.apply_delta(0.0, 99999.0), Crossing::None);
        assert_eq!(s.y, 1439.0);
        // Left edge is not the shared edge here: no crossing.
        assert_eq!(s.target, Target::Local);
    }

    #[test]
    fn crosses_right_edge_with_normalized_y() {
        let mut s = space();
        s.x = 2559.0;
        s.y = 720.0; // vertical middle of local
        assert_eq!(s.apply_delta(5.0, 0.0), Crossing::ToRemote);
        assert_eq!(s.target, Target::Remote);
        assert_eq!(s.x, 0.0);
        assert_eq!(s.y, 540.0); // middle of remote (1080/2)
    }

    #[test]
    fn returns_left_from_remote() {
        let mut s = space();
        s.x = 2559.0;
        s.apply_delta(5.0, 0.0);
        assert_eq!(s.apply_delta(-3.0, 0.0), Crossing::ToLocal);
        assert_eq!(s.target, Target::Local);
        assert_eq!(s.x, 2559.0);
        assert!((s.y - 720.0).abs() < 1.0); // vertical continuity round-trips
    }

    #[test]
    fn canonical_corners() {
        let mut s = space();
        s.x = 2559.0;
        s.apply_delta(5.0, 0.0); // now remote at (0, 540)
        assert_eq!(s.canonical().0, 0);
        s.x = 1919.0;
        s.y = 1079.0;
        assert_eq!(s.canonical(), (65535, 65535));
        s.x = 959.5;
        let cx = s.canonical().0;
        assert!((cx as i32 - 32768).abs() <= 1); // center maps to ~center
    }

    #[test]
    fn force_local_reseats_at_edge() {
        let mut s = space();
        s.x = 2559.0;
        s.apply_delta(5.0, 0.0);
        s.y = 1079.0; // bottom of remote
        s.force_local();
        assert_eq!(s.target, Target::Local);
        assert_eq!(s.x, 2559.0);
        assert!((s.y - 1438.6).abs() < 1.0); // bottom maps to ~bottom
    }

    #[test]
    fn same_size_screens_preserve_y_exactly() {
        let mut s = Space::new(
            Screen { width: 2560.0, height: 1440.0 },
            Screen { width: 2560.0, height: 1440.0 },
            Edge::Right,
        );
        s.x = 2559.0;
        s.y = 700.0;
        s.apply_delta(2.0, 0.0);
        assert_eq!(s.y, 700.0);
    }
}
