use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0);

    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn dot(self, rhs: Self) -> f32 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    pub fn cross(self, rhs: Self) -> Self {
        Self::new(
            self.y * rhs.z - self.z * rhs.y,
            self.z * rhs.x - self.x * rhs.z,
            self.x * rhs.y - self.y * rhs.x,
        )
    }

    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    pub fn normalize_or_zero(self) -> Self {
        let length = self.length();
        if length > 1.0e-6 {
            self / length
        } else {
            Self::ZERO
        }
    }

    pub fn swap_yz(self) -> Self {
        Self::new(self.x, self.z, self.y)
    }
}

impl Add for Vec3 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}
impl AddAssign for Vec3 {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}
impl Sub for Vec3 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}
impl Neg for Vec3 {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}
impl Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self {
        Self::new(self.x * rhs, self.y * rhs, self.z * rhs)
    }
}
impl Div<f32> for Vec3 {
    type Output = Self;
    fn div(self, rhs: f32) -> Self {
        Self::new(self.x / rhs, self.y / rhs, self.z / rhs)
    }
}
impl Mul for Vec3 {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        Self::new(self.x * rhs.x, self.y * rhs.y, self.z * rhs.z)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Box3 {
    pub min: Vec3,
    pub max: Vec3,
}

impl Box3 {
    pub fn new(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }
    pub fn contains_strict(&self, point: Vec3) -> bool {
        point.x > self.min.x
            && point.y > self.min.y
            && point.z > self.min.z
            && point.x < self.max.x
            && point.y < self.max.y
            && point.z < self.max.z
    }
    pub fn swap_yz(self) -> Self {
        Self::new(self.min.swap_yz(), self.max.swap_yz())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Transform {
    pub position: Vec3,
    /// Euler angles passed to GLM's `quat(vec3)` constructor.
    pub rotation: Vec3,
    pub scale: Vec3,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            rotation: Vec3::ZERO,
            scale: Vec3::new(1.0, 1.0, 1.0),
        }
    }
}

impl Transform {
    pub fn point(self, point: Vec3) -> Vec3 {
        self.position + Quat::from_glm_euler(self.rotation).rotate(point * self.scale)
    }

    pub fn normal(self, normal: Vec3) -> Vec3 {
        let unscaled = Vec3::new(
            if self.scale.x != 0.0 {
                normal.x / self.scale.x
            } else {
                0.0
            },
            if self.scale.y != 0.0 {
                normal.y / self.scale.y
            } else {
                0.0
            },
            if self.scale.z != 0.0 {
                normal.z / self.scale.z
            } else {
                0.0
            },
        );
        Quat::from_glm_euler(self.rotation)
            .rotate(unscaled)
            .normalize_or_zero()
    }
}

#[derive(Clone, Copy, Debug)]
struct Quat {
    x: f32,
    y: f32,
    z: f32,
    w: f32,
}

impl Quat {
    /// Formula used by GLM's quaternion constructor from a `vec3` of Euler angles.
    fn from_glm_euler(euler: Vec3) -> Self {
        let c = Vec3::new(
            (euler.x * 0.5).cos(),
            (euler.y * 0.5).cos(),
            (euler.z * 0.5).cos(),
        );
        let s = Vec3::new(
            (euler.x * 0.5).sin(),
            (euler.y * 0.5).sin(),
            (euler.z * 0.5).sin(),
        );
        Self {
            w: c.x * c.y * c.z + s.x * s.y * s.z,
            x: s.x * c.y * c.z - c.x * s.y * s.z,
            y: c.x * s.y * c.z + s.x * c.y * s.z,
            z: c.x * c.y * s.z - s.x * s.y * c.z,
        }
    }

    fn rotate(self, point: Vec3) -> Vec3 {
        let q = Vec3::new(self.x, self.y, self.z);
        point + q.cross(point) * (2.0 * self.w) + q.cross(q.cross(point)) * 2.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Triangle {
    pub a: Vec3,
    pub b: Vec3,
    pub c: Vec3,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_axes() {
        assert_eq!(Vec3::new(1.0, 2.0, 3.0).swap_yz(), Vec3::new(1.0, 3.0, 2.0));
    }

    #[test]
    fn transform_applies_scale_then_rotation_then_translation() {
        let transform = Transform {
            position: Vec3::new(10.0, 0.0, 0.0),
            rotation: Vec3::new(0.0, 0.0, std::f32::consts::FRAC_PI_2),
            scale: Vec3::new(2.0, 1.0, 1.0),
        };
        let actual = transform.point(Vec3::new(1.0, 0.0, 0.0));
        assert!((actual.x - 10.0).abs() < 1e-5);
        assert!((actual.y - 2.0).abs() < 1e-5);
    }
}
