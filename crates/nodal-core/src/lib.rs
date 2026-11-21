//! Nodal core: everything that is pure math or data.
//!
//! Conventions used across the whole engine (CPU, GPU shader and file formats):
//!
//! * World space is left-handed: `+X` right, `+Y` up, `+Z` forward (yaw 0, pitch 0).
//! * `yaw` turns right when positive, `pitch` looks up when positive. Both are
//!   stored in degrees in files and radians in code unless a name says `_deg`.
//! * Equirectangular images map `u = 0.5 + yaw / 2π`, `v = 0.5 - pitch / π`,
//!   so the image centre is straight ahead.
//! * Cube faces are `front(+Z) right(+X) back(-Z) left(-X) up(+Y) down(-Y)`,
//!   each addressed with `u` to the right and `v` downwards as seen from inside.

pub mod cube;
pub mod math;
pub mod projection;
pub mod pyramid;
pub mod tour;
pub mod view;

pub use cube::Face;
pub use glam;
pub use projection::{ProjMode, Projection, Projector};
pub use pyramid::{Pyramid, TileId};
pub use tour::{Action, Hotspot, HotspotKind, Scene, Tour};
pub use view::{View, ViewLimits};
