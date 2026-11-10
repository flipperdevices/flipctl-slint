// Keep a thread on one CPU. The Flipper's fast cores are 4-7; left to the scheduler, the
// decoder shared a fast core with BirriaSDR's USB threads and fell behind.

/// CPUs from a list like "4,5" or "4-5"; None if it cannot be read.
pub fn parse(list: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in list.split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                if b < a {
                    return None;
                }
                cpus.extend(a..=b);
            }
            None => cpus.push(part.trim().parse().ok()?),
        }
    }
    (!cpus.is_empty()).then_some(cpus)
}

/// Pin the calling thread to `cpu`.
pub fn pin(cpu: usize) {
    pin_to(&[cpu]);
}

/// Keep the calling thread, and threads it starts later, on `cpus`.
pub fn pin_to(cpus: &[usize]) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for &cpu in cpus {
            libc::CPU_SET(cpu, &mut set);
        }
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}
