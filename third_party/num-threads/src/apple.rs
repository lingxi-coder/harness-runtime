use std::num::NonZeroUsize;

use libc::{
    kern_return_t, mach_port_t, mach_task_self_, natural_t, task_threads, thread_act_array_t,
    vm_deallocate,
};

#[allow(non_camel_case_types)]
// https://developer.apple.com/documentation/kernel/mach_port_name_t
type mach_port_name_t = natural_t;

extern "C" {
    // https://developer.apple.com/documentation/kernel/1578777-mach_port_deallocate
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_name_t) -> kern_return_t;
}

// Rosetta translates x86_64 processes on Apple silicon, not native arm64 processes.
// Fail closed when the translation status is unavailable on other architectures.
#[cfg(not(target_arch = "aarch64"))]
fn native_execution() -> bool {
    let mut translated: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&translated);
    let result = unsafe {
        libc::sysctlbyname(
            b"sysctl.proc_translated\0".as_ptr() as *const libc::c_char,
            &mut translated as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    translation_query_is_native(result, size, translated)
}

#[cfg(any(not(target_arch = "aarch64"), test))]
fn translation_query_is_native(result: libc::c_int, size: usize, translated: libc::c_int) -> bool {
    result == 0 && size == std::mem::size_of::<libc::c_int>() && translated == 0
}

pub(crate) fn num_threads() -> Option<NonZeroUsize> {
    #[cfg(not(target_arch = "aarch64"))]
    if !native_execution() {
        return None;
    }

    // https://developer.apple.com/documentation/kernel/1537751-task_threads
    let mut thread_list: thread_act_array_t = std::ptr::null_mut();
    let mut thread_count = 0;

    // Safety:
    //  - `mach_task_self_` contains the current task's valid port,
    //  - `thread_list` is a pointer that will point to kernel allocated memory that needs to be
    //    deallocated if the call succeeds
    let task = unsafe { mach_task_self_ };
    let result = unsafe { task_threads(task, &mut thread_list, &mut thread_count) };

    if result == libc::KERN_SUCCESS {
        // Deallocate the mach port rights for the threads
        for thread in 0..thread_count {
            unsafe {
                mach_port_deallocate(task, *(thread_list.offset(thread as isize)) as u32);
            }
        }
        // Deallocate the thread list
        unsafe {
            vm_deallocate(
                task,
                thread_list as usize,
                std::mem::size_of::<mach_port_t>() * thread_count as usize,
            );
        }

        NonZeroUsize::new(thread_count as usize)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::translation_query_is_native;

    #[test]
    fn translated_or_unknown_status_disables_counting() {
        let size = std::mem::size_of::<libc::c_int>();
        assert!(translation_query_is_native(0, size, 0));
        assert!(!translation_query_is_native(0, size, 1));
        assert!(!translation_query_is_native(-1, size, 0));
        assert!(!translation_query_is_native(0, 0, 0));
        assert!(!translation_query_is_native(0, size, 2));
    }
}
