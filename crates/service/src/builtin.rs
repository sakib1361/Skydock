//! App registrations packed into this build by `build.rs`, so users only
//! ever see the provider's own sign-in page.
//!
//! They identify the application, not a user. Anyone holding the binary can
//! read them out, which is expected for an installed app (the Google
//! "secret" included: Google does not treat it as confidential for desktop
//! clients). They are kept out of the source tree only so a fork does not
//! inherit this project's registrations. An empty value means the build was
//! made without one; the GUI then asks for it.

pub const ONEDRIVE_CLIENT_ID: &str = env!("SKYDOCK_BUILTIN_SKYDOCK_ONEDRIVE_CLIENT_ID");

pub const GDRIVE_CLIENT_ID: &str = env!("SKYDOCK_BUILTIN_SKYDOCK_GDRIVE_CLIENT_ID");
pub const GDRIVE_CLIENT_SECRET: &str = env!("SKYDOCK_BUILTIN_SKYDOCK_GDRIVE_CLIENT_SECRET");
