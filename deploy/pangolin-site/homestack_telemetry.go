// SPDX-License-Identifier: AGPL-3.0-only
package site

import (
	"context"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"golang.org/x/sys/unix"
)

type object = map[string]any

type measurement struct {
	at          time.Time
	filesystems []object
	container   object
	err         any
}

type telemetry struct {
	started  time.Time
	mu       sync.Mutex
	sample   measurement
	finished time.Time
	inflight bool
	done     chan struct{}
	requests chan struct{}
	stop     chan struct{}
}

func newTelemetry(started time.Time) *telemetry {
	t := &telemetry{started: started, requests: make(chan struct{}, 1), stop: make(chan struct{}),
		sample: measurement{filesystems: []object{}, container: unavailableContainer("cgroup_unavailable"), err: "pending"}}
	go func() {
		var group *cgroup
		for {
			select {
			case <-t.stop:
				return
			case <-t.requests:
				sample := collect(&group)
				t.mu.Lock()
				t.sample, t.finished, t.inflight = sample, time.Now(), false
				close(t.done)
				t.mu.Unlock()
			}
		}
	}()
	return t
}

func (t *telemetry) close() { close(t.stop) }

func (t *telemetry) report(ctx context.Context, status string) object {
	t.mu.Lock()
	var done chan struct{}
	if t.finished.IsZero() || time.Since(t.finished) >= 2*time.Second {
		if !t.inflight {
			t.inflight, t.done = true, make(chan struct{})
			t.requests <- struct{}{}
		}
		done = t.done
	}
	t.mu.Unlock()
	if done != nil {
		timer := time.NewTimer(700 * time.Millisecond)
		defer timer.Stop()
		select {
		case <-done:
		case <-timer.C:
		case <-ctx.Done():
		}
	}
	t.mu.Lock()
	sample := t.sample
	t.mu.Unlock()
	var uptime, age any
	if !sample.at.IsZero() {
		uptime, age = uint64(sample.at.Sub(t.started)/time.Microsecond), uint64(time.Since(sample.at)/time.Second)
	}
	return object{"schema": 2, "status": status, "uptime_seconds": uint64(time.Since(t.started) / time.Second),
		"sample_uptime_usec": uptime, "sample_age_seconds": age, "filesystems": sample.filesystems,
		"container": sample.container, "error": sample.err}
}

// A single idle worker serves requests; no periodic filesystem or cgroup reads.
// Even a hung NFS call cannot accumulate more workers or hold the HTTP handler.
func collect(group **cgroup) measurement {
	sample := measurement{at: time.Now()}
	text, err := readBounded("/proc/self/mountinfo", 1024*1024)
	mounts, ok := mountEntries(text)
	if err != nil || !ok || len(mounts) == 0 {
		sample.err = "mount_inventory_unavailable"
		mounts = nil
	}
	sample.filesystems, sample.err = filesystems(mounts, sample.err)
	if *group != nil {
		if _, err := os.Stat(filepath.Join((*group).dir, "cgroup.type")); err != nil {
			*group = nil
		}
	}
	code := "cgroup_unavailable"
	if *group == nil {
		*group, code = resolveCgroup(mounts)
	}
	if *group == nil {
		sample.container = unavailableContainer(code)
	} else {
		sample.container = (*group).measure()
	}
	return sample
}

func readBounded(path string, limit int64) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, limit+1))
	if err != nil || int64(len(data)) > limit {
		return "", fmt.Errorf("unavailable")
	}
	return string(data), nil
}

type mountEntry struct{ root, path, kind string }

func unescape(value string) (string, bool) {
	var out strings.Builder
	for i := 0; i < len(value); i++ {
		if value[i] != '\\' {
			out.WriteByte(value[i])
			continue
		}
		if i+3 >= len(value) {
			return "", false
		}
		n, err := strconv.ParseUint(value[i+1:i+4], 8, 8)
		if err != nil || n == 0 {
			return "", false
		}
		out.WriteByte(byte(n))
		i += 3
	}
	return out.String(), true
}

func mountEntries(text string) ([]mountEntry, bool) {
	var entries []mountEntry
	for _, line := range strings.Split(strings.TrimSpace(text), "\n") {
		left, right, ok := strings.Cut(line, " - ")
		fields, kind := strings.Fields(left), strings.Fields(right)
		if !ok || len(fields) < 6 || len(kind) < 3 {
			return nil, false
		}
		root, rootOK := unescape(fields[3])
		path, pathOK := unescape(fields[4])
		if !rootOK || !pathOK || !filepath.IsAbs(root) || !filepath.IsAbs(path) {
			return nil, false
		}
		entries = append(entries, mountEntry{root, path, kind[0]})
	}
	return entries, true
}

func dataMount(path, kind string) bool {
	if path == "/" || path == "/home/app" {
		return true
	}
	for _, base := range []string{"/proc", "/sys", "/dev", "/run"} {
		if path == base || strings.HasPrefix(path, base+"/") {
			return false
		}
	}
	switch kind {
	case "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "securityfs", "debugfs",
		"tracefs", "pstore", "mqueue", "hugetlbfs", "configfs", "fusectl", "autofs":
		return false
	}
	return true
}

func filesystems(entries []mountEntry, inventoryError any) ([]object, any) {
	mounts, textBytes := map[string]string{}, 0
	for _, entry := range entries {
		if !dataMount(entry.path, entry.kind) {
			continue
		}
		if info, err := os.Stat(entry.path); err == nil && !info.IsDir() {
			continue
		}
		textBytes += len(entry.path) + len(entry.kind)
		if len(mounts) >= 126 || textBytes > 4096 {
			inventoryError = "too_many_mounts"
			break
		}
		mounts[entry.path] = entry.kind
	}
	result := make([]object, 0, len(mounts)+2)
	for path, kind := range mounts {
		fs := object{"mount": path, "filesystem": kind, "mounted": true, "total_bytes": nil,
			"used_bytes": nil, "free_bytes": nil, "available_bytes": nil, "error": nil}
		var stat unix.Statfs_t
		if err := unix.Statfs(path, &stat); err != nil {
			fs["error"] = "measurement_unavailable"
		} else {
			unit := uint64(stat.Frsize)
			if stat.Frsize <= 0 || stat.Bfree > stat.Blocks || stat.Bavail > stat.Bfree || stat.Blocks > math.MaxUint64/unit {
				fs["error"] = "invalid_statistics"
			} else {
				fs["total_bytes"], fs["used_bytes"] = stat.Blocks*unit, (stat.Blocks-stat.Bfree)*unit
				fs["free_bytes"], fs["available_bytes"] = stat.Bfree*unit, stat.Bavail*unit
			}
		}
		result = append(result, fs)
	}
	for _, required := range []string{"/", "/home/app"} {
		if _, found := mounts[required]; !found {
			code := "not_mounted"
			if inventoryError != nil {
				code = "mount_inventory_unavailable"
			}
			result = append(result, object{"mount": required, "filesystem": nil, "mounted": false,
				"total_bytes": nil, "used_bytes": nil, "free_bytes": nil, "available_bytes": nil, "error": code})
		}
	}
	sort.Slice(result, func(i, j int) bool { return result[i]["mount"].(string) < result[j]["mount"].(string) })
	return result, inventoryError
}

type cgroup struct{ top, dir string }

func safeAbsolute(path string) bool {
	if !filepath.IsAbs(path) {
		return false
	}
	for _, part := range strings.Split(path, "/") {
		if part == ".." {
			return false
		}
	}
	return true
}

func resolveCgroup(mounts []mountEntry) (*cgroup, string) {
	membership, err := readBounded("/proc/self/cgroup", 64*1024)
	if err != nil {
		return nil, "cgroup_unavailable"
	}
	path := ""
	for _, line := range strings.Split(membership, "\n") {
		if value, ok := strings.CutPrefix(line, "0::"); ok {
			path = value
			break
		}
	}
	if !safeAbsolute(path) {
		return nil, "cgroup_unavailable"
	}
	for _, mount := range mounts {
		if mount.kind != "cgroup2" || !safeAbsolute(mount.root) || !safeAbsolute(mount.path) {
			continue
		}
		relative, err := filepath.Rel(mount.root, path)
		if err != nil || relative == ".." || strings.HasPrefix(relative, "../") {
			continue
		}
		group := &cgroup{mount.path, filepath.Join(mount.path, relative)}
		if _, err := os.Stat(filepath.Join(group.dir, "cgroup.type")); err != nil {
			return nil, "host_cgroup"
		}
		return group, ""
	}
	return nil, "cgroup_unavailable"
}

func unavailableContainer(code string) object {
	return object{"cpu": nil, "memory": nil, "pressure": nil, "io": nil, "error": code}
}

func (g *cgroup) read(name string) string {
	text, _ := readBounded(filepath.Join(g.dir, name), 64*1024)
	return text
}

func keyed(text string) map[string]uint64 {
	result := map[string]uint64{}
	for _, line := range strings.Split(text, "\n") {
		fields := strings.Fields(line)
		if len(fields) == 2 {
			if n, err := strconv.ParseUint(fields[1], 10, 64); err == nil {
				result[fields[0]] = n
			}
		}
	}
	return result
}

func counter(fields map[string]uint64, key string) any {
	if n, ok := fields[key]; ok {
		return n
	}
	return nil
}

func counters(fields map[string]uint64, names map[string]string) any {
	result := object{}
	for output, input := range names {
		if _, ok := fields[input]; !ok {
			return nil
		}
		result[output] = fields[input]
	}
	return result
}

func (g *cgroup) ancestors(visit func(string)) {
	for dir := g.dir; ; dir = filepath.Dir(dir) {
		visit(dir)
		if dir == g.top || filepath.Dir(dir) == dir {
			break
		}
	}
}

func (g *cgroup) measure() object {
	var ioUsage any
	if text, err := readBounded(filepath.Join(g.dir, "io.stat"), 64*1024); err == nil {
		ioUsage = ioCounters(text)
	}
	result := object{"cpu": nil, "memory": nil, "pressure": object{
		"cpu": psi(g.read("cpu.pressure")), "memory": psi(g.read("memory.pressure")), "io": psi(g.read("io.pressure"))},
		"io": ioUsage, "error": nil}
	cpu := counters(keyed(g.read("cpu.stat")), map[string]string{
		"usage_usec": "usage_usec", "user_usec": "user_usec", "system_usec": "system_usec"})
	if cpu != nil {
		value := cpu.(object)
		value["capacity_cpus"] = g.capacity()
		value["throttling"] = counters(keyed(g.read("cpu.stat")), map[string]string{
			"periods": "nr_periods", "throttled_periods": "nr_throttled", "throttled_usec": "throttled_usec"})
		result["cpu"] = value
	}
	if usage, err := strconv.ParseUint(strings.TrimSpace(g.read("memory.current")), 10, 64); err == nil {
		stat := keyed(g.read("memory.stat"))
		var limit any
		g.ancestors(func(dir string) {
			text, _ := readBounded(filepath.Join(dir, "memory.max"), 64*1024)
			if n, err := strconv.ParseUint(strings.TrimSpace(text), 10, 64); err == nil && n > 0 {
				if limit == nil || n < limit.(uint64) {
					limit = n
				}
			}
		})
		result["memory"] = object{"usage_bytes": usage, "limit_bytes": limit,
			"anon_bytes": counter(stat, "anon"), "file_bytes": counter(stat, "file"), "process_rss_bytes": processRSS(),
			"events": counters(keyed(g.read("memory.events")), map[string]string{
				"high": "high", "max": "max", "oom": "oom", "oom_kill": "oom_kill"})}
	}
	return result
}

func (g *cgroup) capacity() float64 {
	var affinity unix.CPUSet
	count := runtime.NumCPU()
	if unix.SchedGetaffinity(0, &affinity) == nil && affinity.Count() > 0 {
		count = affinity.Count()
	}
	if text := strings.TrimSpace(g.read("cpuset.cpus.effective")); text != "" {
		parsed := 0
		for _, span := range strings.Split(text, ",") {
			first, last, found := strings.Cut(span, "-")
			if !found {
				last = first
			}
			a, errA := strconv.Atoi(first)
			b, errB := strconv.Atoi(last)
			if errA != nil || errB != nil || a < 0 || b < a || b > 65535 {
				parsed = 0
				break
			}
			parsed += b - a + 1
		}
		if parsed > 0 {
			count = parsed
		}
	}
	capacity := float64(count)
	g.ancestors(func(dir string) {
		text, _ := readBounded(filepath.Join(dir, "cpu.max"), 64*1024)
		fields := strings.Fields(text)
		if len(fields) != 2 {
			return
		}
		quota, errQ := strconv.ParseUint(fields[0], 10, 64)
		period, errP := strconv.ParseUint(fields[1], 10, 64)
		if errQ == nil && errP == nil && period > 0 && quota > 0 {
			capacity = math.Min(capacity, float64(quota)/float64(period))
		}
	})
	return capacity
}

func processRSS() any {
	text, _ := readBounded("/proc/self/status", 64*1024)
	for _, line := range strings.Split(text, "\n") {
		if value, ok := strings.CutPrefix(line, "VmRSS:"); ok {
			fields := strings.Fields(value)
			if len(fields) == 2 && fields[1] == "kB" {
				if n, err := strconv.ParseUint(fields[0], 10, 64); err == nil && n <= math.MaxUint64/1024 {
					return n * 1024
				}
			}
		}
	}
	return nil
}

func psi(text string) any {
	result := object{"some": nil, "full": nil}
	for _, line := range strings.Split(text, "\n") {
		fields := strings.Fields(line)
		if len(fields) < 5 || (fields[0] != "some" && fields[0] != "full") {
			continue
		}
		values := map[string]string{}
		for _, field := range fields[1:] {
			key, value, _ := strings.Cut(field, "=")
			values[key] = value
		}
		item := object{}
		for _, key := range []string{"avg10", "avg60", "avg300"} {
			value, err := strconv.ParseFloat(values[key], 64)
			if err != nil || math.IsNaN(value) || math.IsInf(value, 0) || value < 0 || value > 100 {
				return nil
			}
			item[key] = value
		}
		total, err := strconv.ParseUint(values["total"], 10, 64)
		if err != nil {
			return nil
		}
		item["total_usec"] = total
		result[fields[0]] = item
	}
	if result["some"] == nil {
		return nil
	}
	return result
}

func ioCounters(text string) any {
	result := object{"read_bytes": uint64(0), "write_bytes": uint64(0), "read_ops": uint64(0), "write_ops": uint64(0)}
	if strings.TrimSpace(text) == "" {
		return result
	}
	names := map[string]string{"rbytes": "read_bytes", "wbytes": "write_bytes", "rios": "read_ops", "wios": "write_ops"}
	for _, line := range strings.Split(strings.TrimSpace(text), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 5 {
			return nil
		}
		seen := map[string]bool{}
		for _, field := range fields[1:] {
			key, value, _ := strings.Cut(field, "=")
			output, wanted := names[key]
			if !wanted {
				continue
			}
			n, err := strconv.ParseUint(value, 10, 64)
			old := result[output].(uint64)
			if err != nil || n > math.MaxUint64-old || seen[output] {
				return nil
			}
			result[output], seen[output] = old+n, true
		}
		if len(seen) != 4 {
			return nil
		}
	}
	return result
}
