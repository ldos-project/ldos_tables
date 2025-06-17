//! Utilities that provide the equivalent of OS functionality in a test environment.

use std::thread::yield_now;

pub fn yield_thread() {
    yield_now();
}