use serial_test::serial;
use std::sync::mpsc;
use std::{thread, time};
use tacky_borders::utils::{
    WM_APP_LOCATIONCHANGE, create_border_for_window, get_border_for_window, get_window_class,
    get_window_rule, is_window_visible, send_message_w, set_process_dpi_awareness_context,
};
use tacky_borders::{
    create_borders_for_existing_windows, destroy_borders, register_border_window_class,
    reload_borders,
};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, TRUE, WPARAM};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, EnumWindows, GetMessageW, GetWindowRect,
    GetWindowThreadProcessId, MSG, PostThreadMessageW, WINDOW_EX_STYLE, WM_DPICHANGED, WM_QUIT,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::{BOOL, w};

#[test]
#[serial]
fn test_destroy_borders() -> anyhow::Result<()> {
    register_border_window_class()?;

    for _ in 0..5 {
        // This is kinda jank becauuse we have to wait for the border windows to actually be created
        // (they're all in separate threads), but 50ms should be more than long enough for that.
        create_borders_for_existing_windows()?;
        thread::sleep(time::Duration::from_millis(50));

        destroy_borders();

        unsafe { EnumWindows(Some(enum_windows_tests_callback), LPARAM::default()) }?;
    }

    Ok(())
}

#[test]
#[serial]
// This tests whether all borders are properly cleaned up when reload_borders() is called
fn test_reload_borders() -> anyhow::Result<()> {
    register_border_window_class()?;
    create_borders_for_existing_windows()?;

    for _ in 0..5 {
        reload_borders();
    }
    destroy_borders();

    unsafe { EnumWindows(Some(enum_windows_tests_callback), LPARAM::default()) }?;

    Ok(())
}

#[test]
#[serial]
// The border's width and offset scale with DPI and are part of the border window's rect, so a DPI
// change must reposition the border without waiting for the tracking window to move.
fn test_border_repositions_on_dpi_change() -> anyhow::Result<()> {
    // Match the real app, which is per-monitor DPI aware. This fails if it was already set.
    let _ = set_process_dpi_awareness_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    register_border_window_class()?;

    // The tracking window needs a thread that pumps its messages: creating a border sends it
    // messages (e.g. WM_GETTEXT for its title), which would deadlock against this thread.
    let (hwnd_sender, hwnd_receiver) = mpsc::channel::<isize>();
    let tracking_thread = thread::spawn(move || unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("tacky-borders dpi test"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            200,
            200,
            600,
            400,
            None,
            None,
            None,
            None,
        )
        .expect("could not create tracking window");
        hwnd_sender.send(hwnd.0 as isize).unwrap();

        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            DispatchMessageW(&message);
        }
        let _ = DestroyWindow(hwnd);
    });
    let tracking_window = HWND(hwnd_receiver.recv_timeout(time::Duration::from_secs(5))? as _);
    let tracking_thread_id = unsafe { GetWindowThreadProcessId(tracking_window, None) };

    create_border_for_window(tracking_window, get_window_rule(tracking_window));

    // The border is created and shown in a separate thread, so wait for it
    let mut border_window = None;
    for _ in 0..100 {
        border_window =
            get_border_for_window(tracking_window).filter(|hwnd| is_window_visible(*hwnd));
        if border_window.is_some() {
            break;
        }
        thread::sleep(time::Duration::from_millis(50));
    }
    let border_window = border_window.expect("border was not shown");

    let get_rect = || -> anyhow::Result<RECT> {
        let mut rect = RECT::default();
        unsafe { GetWindowRect(border_window, &mut rect) }?;
        Ok(rect)
    };

    let old_dpi = unsafe { GetDpiForWindow(border_window) } as usize;
    let new_dpi = if old_dpi == 96 { 192 } else { 96 };

    // The LPARAM has to point to a (suggested) rect, otherwise the message doesn't get delivered
    let rect_before = get_rect()?;
    send_message_w(
        border_window,
        WM_DPICHANGED,
        Some(WPARAM((new_dpi << 16) | new_dpi)),
        Some(LPARAM(&rect_before as *const RECT as isize)),
    );
    let rect_after_dpi_change = get_rect()?;

    // A location change event does a full resync, so it shouldn't have anything left to fix
    send_message_w(border_window, WM_APP_LOCATIONCHANGE, None, None);
    let rect_after_location_change = get_rect()?;

    destroy_borders();
    unsafe { PostThreadMessageW(tracking_thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }?;
    tracking_thread
        .join()
        .expect("tracking window thread panicked");

    assert_ne!(
        rect_before, rect_after_location_change,
        "the DPI change did not affect the border's rect, so this test checks nothing"
    );
    assert_eq!(rect_after_dpi_change, rect_after_location_change);

    Ok(())
}

unsafe extern "system" fn enum_windows_tests_callback(_hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let window_class = get_window_class(_hwnd).unwrap();
    assert!(window_class != "border");

    TRUE
}

// TODO: test border window rect with positive border offsets and negative effects translations
