// SPDX-License-Identifier: AGPL-3.0-only
package site

import (
	"io"
	"math"
	"os"
	"strconv"
	"strings"
)

// HomeStack service status schema 3: liveness and this process's RSS.
// Container resources, filesystems and uptime come from the PVE agent.
func statusReport(status string) map[string]any {
	return map[string]any{"schema": 3, "status": status, "process_rss_bytes": processRSS()}
}

func processRSS() any {
	file, err := os.Open("/proc/self/status")
	if err != nil {
		return nil
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, 64*1024))
	if err != nil {
		return nil
	}
	for _, line := range strings.Split(string(data), "\n") {
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
