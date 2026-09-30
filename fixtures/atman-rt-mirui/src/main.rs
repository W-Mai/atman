#[cfg(feature = "sdl")]
fn main() {
    match atman_rt_mirui::run_sdl_demo() {
        Ok(atman_rt_mirui::SdlRun::Completed(hashes)) => {
            println!("mirui SDL rendered {} VM frames: {hashes:?}", hashes.len())
        }
        Ok(atman_rt_mirui::SdlRun::ClosedEarly { rendered_frames }) => {
            println!("mirui SDL window closed after {rendered_frames} scripted frames")
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(feature = "sdl"))]
fn main() {
    match atman_rt_mirui::run_headless_demo() {
        Ok(hashes) => println!(
            "mirui framebuffer rendered {} VM frames: {hashes:?}",
            hashes.len()
        ),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
