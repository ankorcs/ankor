//! Memory, CPU and disk readings, the ring of ticks, and the frames that carry them while the controller
//! watches.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::node_log::Transport;
use ankor_agent::resources::{
    Flushed, FsStats, Meminfo, ResourceBuffer, ResourcePaths, ResourceReporter, ResourceReporterOptions, ResourceSampler, ResourceStatus, Sampler, disk_usage,
    parse_meminfo, parse_os_release, read_os_name,
};
use ankor_protocol::constants::{OS_NAME_MAX_CHARS, RESOURCE_TICK};
use ankor_protocol::types::{AgentResourceBatch, AgentToControllerMessage, ResourceTick, ResourceTickCpu, ResourceUsage};
use tempfile::TempDir;

const MEMINFO: &str = "MemTotal:        1048576 kB\nMemFree:            9484 kB\nMemAvailable:     584290 kB\n";

/// The files an LXC's root cgroup shows, as the node they were read from had them.
fn cgroup(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();

    for (name, content) in files {
        std::fs::write(dir.path().join(name), content).unwrap();
    }

    dir
}

fn paths(dir: &Path) -> ResourcePaths {
    ResourcePaths { cgroup: dir.to_owned(), meminfo: vec![dir.join("meminfo")], stat: dir.join("stat") }
}

fn write(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

fn usage(cap: i64, used: i64) -> ResourceUsage {
    ResourceUsage { cap, used }
}

fn cpu(max: i64, average: i64) -> ResourceTickCpu {
    ResourceTickCpu { max, average }
}

mod resources {
    use super::*;

    #[test]
    fn reads_total_and_available_from_meminfo_in_bytes() {
        assert_eq!(parse_meminfo(MEMINFO), Some(Meminfo { total_bytes: 1_073_741_824, available_bytes: 598_312_960 }));
        assert_eq!(parse_meminfo("MemTotal:        1048576 kB\n"), None);
        assert_eq!(parse_meminfo("MemAvailable:     584290 kB\n"), None);
    }

    #[tokio::test]
    async fn counts_memory_the_way_the_platform_gauge_does_resident_less_droppable_file_cache_against_the_limit() {
        let dir = cgroup(&[
            ("memory.current", "1049485312\n"),
            ("memory.max", "max\n"),
            ("memory.stat", "anon 442359808\nfile 539332608\ninactive_file 424681472\nactive_file 114266112\n"),
            ("cpu.stat", "usage_usec 2802192238\nuser_usec 2661146296\n"),
            ("cpu.max", "max 100000\n"),
            ("meminfo", MEMINFO),
        ]);
        let mut sampler = ResourceSampler::new(paths(dir.path()), 1);

        // No limit in the cgroup: the total is what meminfo says the environment has.
        assert_eq!(sampler.report(1_000).await, ResourceStatus { memory: usage(1_073_741_824, 624_803_840), cpu: None });
    }

    #[tokio::test]
    async fn takes_a_numeric_memory_max_as_the_total() {
        let dir =
            cgroup(&[("memory.current", "300000000\n"), ("memory.max", "2147483648\n"), ("memory.stat", "inactive_file 100000000\n"), ("meminfo", MEMINFO)]);

        assert_eq!(ResourceSampler::new(paths(dir.path()), 1).report(1_000).await.memory, usage(2_147_483_648, 200_000_000));
    }

    #[tokio::test]
    async fn measures_cpu_as_busy_time_over_wall_time_across_the_cpus_the_environment_may_use() {
        let dir = cgroup(&[
            ("memory.current", "1\n"),
            ("memory.max", "10\n"),
            ("memory.stat", ""),
            ("cpu.stat", "usage_usec 1000000\n"),
            ("cpu.max", "max 100000\n"),
            ("meminfo", MEMINFO),
        ]);
        let mut sampler = ResourceSampler::new(paths(dir.path()), 2);

        assert_eq!(sampler.report(0).await.cpu, None);
        // 10 s later, 5 s of cpu were used on two cpus: a quarter of the budget.
        write(dir.path(), "cpu.stat", "usage_usec 6000000\n");
        sampler.sample_cpu(10_000).await;
        // Another 10 s, all of both cpus busy: the report carries both samples.
        write(dir.path(), "cpu.stat", "usage_usec 26000000\n");
        assert_eq!(sampler.report(20_000).await.cpu, Some(cpu(100, 63)));
        // The window empties with the report: the next one carries only its own idle sample.
        assert_eq!(sampler.report(20_001).await.cpu, Some(cpu(0, 0)));
        // No time has passed, so there is no budget to measure against.
        assert_eq!(sampler.report(20_001).await.cpu, None);
    }

    #[tokio::test]
    async fn honours_a_cpu_quota_narrower_than_the_visible_cpus() {
        let dir = cgroup(&[
            ("memory.current", "1\n"),
            ("memory.max", "10\n"),
            ("memory.stat", ""),
            ("cpu.stat", "usage_usec 0\n"),
            ("cpu.max", "50000 100000\n"),
            ("meminfo", MEMINFO),
        ]);
        let mut sampler = ResourceSampler::new(paths(dir.path()), 8);

        sampler.report(0).await;
        // Half a cpu of quota, fully used for one second.
        write(dir.path(), "cpu.stat", "usage_usec 500000\n");
        assert_eq!(sampler.report(1_000).await.cpu, Some(cpu(100, 100)));
    }

    #[tokio::test]
    async fn measures_against_the_visible_cpus_where_the_cgroup_has_no_cpu_max_as_the_root_cgroup_has_not() {
        let dir = cgroup(&[("meminfo", MEMINFO), ("cpu.stat", "usage_usec 0\n")]);
        let mut sampler = ResourceSampler::new(paths(dir.path()), 2);

        sampler.report(0).await;
        // One cpu's worth of a second, on two cpus.
        write(dir.path(), "cpu.stat", "usage_usec 1000000\n");
        assert_eq!(sampler.report(1_000).await.cpu, Some(cpu(50, 50)));
    }

    #[tokio::test]
    async fn falls_back_to_meminfo_and_proc_stat_where_there_is_no_cgroup_accounting() {
        // Bare metal: the root cgroup has memory.stat but no memory.current, which only children carry.
        let dir = cgroup(&[("meminfo", MEMINFO), ("memory.stat", "inactive_file 1\n"), ("stat", "cpu  100 20 100 700 50 0 0 0 0 0\n")]);
        let mut sampler = ResourceSampler::new(paths(dir.path()), 1);

        assert_eq!(sampler.report(0).await, ResourceStatus { memory: usage(1_073_741_824, 475_428_864), cpu: None });
        write(dir.path(), "stat", "cpu  200 40 200 1400 100 0 0 0 0 0\n");
        // Since the last read: 220 busy of 970 total jiffies; waiting on I/O is idle.
        assert_eq!(sampler.report(1_000).await.cpu, Some(cpu(23, 23)));
    }

    /// A host that runs VMs counts their time in `user` and `nice` and again as `guest`; the second
    /// count is left out.
    #[tokio::test]
    async fn counts_guest_time_once() {
        let dir = cgroup(&[("meminfo", MEMINFO), ("memory.stat", "inactive_file 1\n"), ("stat", "cpu  100 20 100 700 50 0 0 0 0 0\n")]);
        let mut sampler = ResourceSampler::new(paths(dir.path()), 1);

        sampler.report(0).await;
        write(dir.path(), "stat", "cpu  200 40 200 1400 100 0 0 0 80 20\n");
        assert_eq!(sampler.report(1_000).await.cpu, Some(cpu(23, 23)));
    }

    #[tokio::test]
    async fn takes_a_signed_memory_max_as_no_limit() {
        for max in ["-5\n", "+2147483648\n"] {
            let dir = cgroup(&[("memory.current", "300000000\n"), ("memory.max", max), ("memory.stat", "inactive_file 100000000\n"), ("meminfo", MEMINFO)]);

            assert_eq!(ResourceSampler::new(paths(dir.path()), 1).report(1_000).await.memory, usage(1_073_741_824, 200_000_000), "{max:?}");
        }
    }

    /// Linux only: elsewhere the kernel's figures are not read, and memory reads as zero.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reads_the_kernels_memory_figures_when_no_meminfo_is_readable() {
        let dir = cgroup(&[]);
        let memory = ResourceSampler::new(paths(dir.path()), 1).report(0).await.memory;

        assert!(memory.cap > 0 && memory.used >= 0, "{memory:?}");
    }

    #[tokio::test]
    async fn falls_back_to_meminfo_when_the_cgroup_has_a_memory_current_but_no_memory_stat() {
        let dir = cgroup(&[("memory.current", "1000\n"), ("meminfo", MEMINFO)]);

        assert_eq!(ResourceSampler::new(paths(dir.path()), 1).report(0).await.memory, usage(1_073_741_824, 475_428_864));
    }

    #[tokio::test]
    async fn reads_a_proc_stat_cpu_line_of_only_the_four_oldest_fields() {
        let dir = cgroup(&[("meminfo", MEMINFO), ("stat", "cpu  100 0 100 700\n")]);
        let mut sampler = ResourceSampler::new(paths(dir.path()), 1);

        sampler.report(0).await;
        write(dir.path(), "stat", "cpu  200 0 200 1400\n");
        assert_eq!(sampler.report(1_000).await.cpu, Some(cpu(22, 22)));
    }

    #[tokio::test]
    async fn takes_no_cpu_sample_from_a_proc_stat_it_cannot_read() {
        let dir = cgroup(&[("meminfo", MEMINFO), ("stat", "cpu  100 0 100\n")]);
        let mut sampler = ResourceSampler::new(paths(dir.path()), 1);

        sampler.report(0).await;
        write(dir.path(), "stat", "cpu  200 0 200\n");
        assert_eq!(sampler.report(1_000).await.cpu, None);
        write(dir.path(), "stat", "cpu  a b c d\n");
        sampler.report(2_000).await;
        write(dir.path(), "stat", "cpu  e f g h\n");
        assert_eq!(sampler.report(3_000).await.cpu, None);
    }

    #[tokio::test]
    async fn skips_a_garbled_proc_stat_between_two_good_ones_and_measures_the_next_against_the_last_good_read() {
        let dir = cgroup(&[("meminfo", MEMINFO), ("stat", "cpu  100 20 100 700 50 0 0 0 0 0\n")]);
        let mut sampler = ResourceSampler::new(paths(dir.path()), 1);

        sampler.report(0).await;
        write(dir.path(), "stat", "cpu  150 20 150 x 50\n");
        assert_eq!(sampler.report(1_000).await.cpu, None);
        write(dir.path(), "stat", "cpu  200 40 200 1400 100 0 0 0 0 0\n");
        assert_eq!(sampler.report(2_000).await.cpu, Some(cpu(23, 23)));
    }
}

mod resource_buffer {
    use super::*;

    fn tick(at: i64) -> ResourceTick {
        ResourceTick { at, disk: None, memory: usage(10, 1), cpu: None }
    }

    fn ats(flushed: Option<Flushed>) -> Option<(Vec<i64>, bool)> {
        flushed.map(|flushed| (flushed.ticks.iter().map(|tick| tick.at).collect(), flushed.reset))
    }

    fn buffer() -> ResourceBuffer {
        ResourceBuffer::new(3, Duration::from_mins(2))
    }

    #[test]
    fn holds_only_the_last_ticks_and_sends_nothing_while_nobody_watches() {
        let mut buffer = buffer();

        assert_eq!(buffer.flush(0, true), None);
        buffer.push(tick(3_000));
        buffer.push(tick(6_000));
        buffer.push(tick(9_000));
        buffer.push(tick(12_000));
        assert_eq!(buffer.flush(12_000, false), None);
        assert_eq!(buffer.flush(900_000, false), None);
        assert_eq!(ats(buffer.flush(900_000, true)), Some((vec![6_000, 9_000, 12_000], true)));
    }

    #[test]
    fn sends_the_whole_ring_first_then_only_new_ticks_while_a_watch_lease_runs_then_falls_silent_by_itself() {
        let mut buffer = buffer();

        buffer.push(tick(0));
        // The first ping starts the lease; a burst from several tabs only moves its end.
        assert!(buffer.watch(1_000));
        assert!(!buffer.watch(1_001));
        assert!(!buffer.watch(1_002));
        buffer.push(tick(3_000));
        assert_eq!(ats(buffer.flush(3_000, false)), Some((vec![0, 3_000], true)));
        // Nothing new: nothing to send.
        assert_eq!(buffer.flush(3_500, false), None);
        buffer.push(tick(120_000));
        assert_eq!(ats(buffer.flush(120_000, false)), Some((vec![120_000], false)));
        // 120 s past the last ping the lease is over.
        buffer.push(tick(121_002));
        assert_eq!(buffer.flush(121_002, false), None);
        assert!(!buffer.watched(121_002));
        assert!(buffer.watch(121_003));
        // The new lease starts with the whole ring, forced the way the agent does it.
        assert_eq!(ats(buffer.flush(121_003, true)), Some((vec![3_000, 120_000, 121_002], true)));
    }

    #[test]
    fn sends_each_new_tick_after_the_wall_clock_steps_back() {
        let mut buffer = buffer();

        buffer.watch(0);
        buffer.push(tick(9_000));
        assert_eq!(ats(buffer.flush(9_000, false)), Some((vec![9_000], true)));
        // NTP steps the clock back by five seconds.
        buffer.push(tick(4_000));
        assert_eq!(ats(buffer.flush(4_000, false)), Some((vec![4_000], false)));
        buffer.push(tick(7_000));
        assert_eq!(ats(buffer.flush(7_000, false)), Some((vec![7_000], false)));
    }

    #[test]
    fn sends_the_ticks_pushed_between_two_flushes_once_each_and_no_more_than_the_ring_holds() {
        let mut buffer = buffer();

        buffer.watch(0);
        buffer.push(tick(1_000));
        assert_eq!(ats(buffer.flush(1_000, false)), Some((vec![1_000], true)));
        buffer.push(tick(2_000));
        buffer.push(tick(3_000));
        assert_eq!(ats(buffer.flush(3_000, false)), Some((vec![2_000, 3_000], false)));
        assert_eq!(buffer.flush(3_000, false), None);

        // Five ticks while no flush ran: the ring kept the last three.
        for at in [4_000, 5_000, 6_000, 7_000, 8_000] {
            buffer.push(tick(at));
        }

        assert_eq!(ats(buffer.flush(8_000, false)), Some((vec![6_000, 7_000, 8_000], false)));
    }

    #[test]
    fn sends_the_whole_ring_again_after_resend_for_a_connection_that_holds_nothing() {
        let mut buffer = buffer();

        buffer.watch(0);
        buffer.push(tick(3_000));
        assert_eq!(ats(buffer.flush(3_000, false)), Some((vec![3_000], true)));
        buffer.push(tick(6_000));
        buffer.resend();
        assert_eq!(ats(buffer.flush(6_000, false)), Some((vec![3_000, 6_000], true)));
    }
}

mod resource_reporter {
    use super::*;

    struct CountingSampler(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Sampler for CountingSampler {
        async fn sample_cpu(&mut self, _: i64) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        async fn report(&mut self, _: i64) -> ResourceStatus {
            ResourceStatus { memory: usage(10, 1), cpu: None }
        }
    }

    struct Frames {
        connected: bool,
        sent: Mutex<Vec<AgentResourceBatch>>,
    }

    impl Transport for Frames {
        fn connected(&self) -> bool {
            self.connected
        }

        fn send(&self, frame: AgentToControllerMessage) {
            let AgentToControllerMessage::ResourceBatch(batch) = frame else { panic!("{frame:?}") };

            self.sent.lock().unwrap().push(batch);
        }
    }

    struct Harness {
        reporter: ResourceReporter,
        frames: Arc<Frames>,
        cpu_samples: Arc<AtomicUsize>,
        stop: tokio_util::sync::CancellationToken,
    }

    impl Harness {
        fn frames(&self) -> Vec<AgentResourceBatch> {
            self.frames.sent.lock().unwrap().clone()
        }

        fn cpu_samples(&self) -> usize {
            self.cpu_samples.load(Ordering::SeqCst)
        }
    }

    fn reporter(connected: bool) -> Harness {
        let frames = Arc::new(Frames { connected, sent: Mutex::default() });
        let cpu_samples = Arc::new(AtomicUsize::new(0));
        let start = tokio::time::Instant::now();
        let stop = tokio_util::sync::CancellationToken::new();
        let reporter = ResourceReporter::new(ResourceReporterOptions {
            transport: Arc::clone(&frames) as Arc<dyn Transport>,
            sampler: Box::new(CountingSampler(Arc::clone(&cpu_samples))),
            read_disk: Arc::new(|| Some(usage(100, 50))),
            now: Arc::new(move || i64::try_from(start.elapsed().as_millis()).unwrap()),
            stop: stop.clone(),
        });

        Harness { reporter, frames, cpu_samples, stop }
    }

    /// Moves paused time on by `ms`, running each timer that falls due in order.
    async fn advance(by: Duration) {
        tokio::time::sleep(by).await;

        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    fn tick_ms(count: i64) -> i64 {
        i64::try_from(RESOURCE_TICK.as_millis()).unwrap() * count
    }

    #[tokio::test(start_paused = true)]
    async fn samples_the_cpu_between_ticks_and_sends_the_whole_ring_when_a_watch_starts_then_each_new_tick() {
        let test = reporter(true);
        let resources = &test.reporter;

        resources.start();
        advance(RESOURCE_TICK).await;
        // Unwatched: sampled and kept, not sent.
        assert_eq!(test.cpu_samples(), usize::try_from(RESOURCE_TICK.as_millis() / 500 - 1).unwrap());
        assert!(test.frames().is_empty());

        resources.watch(false);
        assert_eq!(
            test.frames(),
            [AgentResourceBatch { reset: true, ticks: vec![ResourceTick { at: tick_ms(1), disk: Some(usage(100, 50)), memory: usage(10, 1), cpu: None }] }]
        );

        advance(RESOURCE_TICK).await;
        let last = test.frames().last().cloned().unwrap();
        assert!(!last.reset);
        assert_eq!(last.ticks.iter().map(|tick| tick.at).collect::<Vec<_>>(), [tick_ms(2)]);

        // A replay while the lease runs sends the whole ring at once.
        resources.watch(true);
        let last = test.frames().last().cloned().unwrap();
        assert!(last.reset);
        assert_eq!(last.ticks.len(), 2);

        // A new connection gets the whole ring on the next tick.
        resources.resend();
        advance(RESOURCE_TICK).await;
        let last = test.frames().last().cloned().unwrap();
        assert!(last.reset);
        assert_eq!(last.ticks.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn samples_on_one_timer_however_often_it_is_started_and_not_at_all_once_cancelled() {
        let test = reporter(true);
        let resources = &test.reporter;

        resources.start();
        resources.start();
        advance(Duration::from_secs(1)).await;
        assert_eq!(test.cpu_samples(), 2);
        test.stop.cancel();
        advance(RESOURCE_TICK).await;
        assert_eq!(test.cpu_samples(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn sends_nothing_while_the_connection_would_not_carry_it() {
        let test = reporter(false);
        let resources = &test.reporter;

        resources.start();
        resources.watch(false);
        advance(RESOURCE_TICK * 2).await;
        assert!(test.frames().is_empty());
    }
}

mod disk_usage {
    use super::*;

    #[test]
    fn counts_the_filesystem_the_way_df_does() {
        assert_eq!(disk_usage(&FsStats { frsize: 4096, blocks: 1000, bfree: 250, bavail: 200 }), usage(4_096_000, 4096 * 750));
    }
}

mod parse_os_release {
    use super::*;

    #[test]
    fn reads_pretty_name_unquoted_and_falls_back_to_name() {
        assert_eq!(
            parse_os_release("NAME=\"Debian GNU/Linux\"\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nID=debian\n").as_deref(),
            Some("Debian GNU/Linux 13 (trixie)")
        );
        assert_eq!(parse_os_release("NAME='Arch Linux'\nID=arch\n").as_deref(), Some("Arch Linux"));
        assert_eq!(parse_os_release("ID=custom\n"), None);
    }

    #[test]
    fn trims_a_value_strips_only_quotes_around_all_of_it_and_skips_an_empty_one() {
        assert_eq!(parse_os_release("PRETTY_NAME=Alpine Linux v3.22  \n").as_deref(), Some("Alpine Linux v3.22"));
        assert_eq!(parse_os_release("PRETTY_NAME=Ubuntu \"Noble\"\n").as_deref(), Some("Ubuntu \"Noble\""));
        assert_eq!(parse_os_release("PRETTY_NAME=\"Ubuntu\" LTS\n").as_deref(), Some("\"Ubuntu\" LTS"));
        assert_eq!(parse_os_release("PRETTY_NAME=\"\"\nNAME=Debian\n").as_deref(), Some("Debian"));
    }

    #[test]
    fn cuts_a_name_longer_than_hello_takes() {
        let longest = usize::try_from(OS_NAME_MAX_CHARS).unwrap();

        assert_eq!(parse_os_release(&format!("PRETTY_NAME={}\n", "x".repeat(longest + 1))), Some("x".repeat(longest)));
    }

    #[tokio::test]
    async fn reads_the_file_node_setup_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("os-release");

        std::fs::write(&file, "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n").unwrap();
        assert_eq!(read_os_name(&file).await.as_deref(), Some("Debian GNU/Linux 13 (trixie)"));
    }

    #[tokio::test]
    async fn is_null_when_node_setup_did_not_mount_the_file() {
        assert_eq!(read_os_name(&std::env::temp_dir().join("ankor-missing-os-release")).await, None);
    }
}
