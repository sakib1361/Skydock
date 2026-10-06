//! Keeps the binary runnable on older glibc than the build machine has.
//!
//! glibc 2.43 gave `acosf` and `atan2f` new symbol versions, so a binary
//! linked on a 2.43 system refuses to start on anything older, although it
//! needs nothing new from them. Defining the two functions here makes the
//! linker bind to these instead of glibc's. `scripts/package.sh` checks the
//! resulting requirement, so a future symbol of this kind fails the build
//! rather than a user's launch.

#[unsafe(no_mangle)]
extern "C" fn acosf(x: f32) -> f32 {
    libm::acosf(x)
}

#[unsafe(no_mangle)]
extern "C" fn atan2f(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}
