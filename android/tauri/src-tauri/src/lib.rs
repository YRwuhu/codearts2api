#[cfg(target_os = "android")]
mod app;
#[cfg(any(target_os = "android", test))]
mod gateway;

#[cfg(target_os = "android")]
pub use app::run;
