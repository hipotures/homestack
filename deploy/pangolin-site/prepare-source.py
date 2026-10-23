#!/usr/bin/env python3
"""Add native HomeStack endpoints to the pinned Pangolin CLI and Newt sources."""

from pathlib import Path
import shutil
import sys


def replace(path, old, new, count=1):
    text = path.read_text()
    if text.count(old) != count:
        raise SystemExit(f"Unexpected upstream source at {path.name}")
    path.write_text(text.replace(old, new))


root = Path(sys.argv[1])
newt = Path(sys.argv[2])
site = root / "cmd/up/site"
for source in Path(__file__).parent.glob("homestack_*.go"):
    shutil.copyfile(source, site / source.name)
replace(site / "site.go", "\tn.Start(sigCtx)", """\tstopHomeStack, err := startHomeStack(sigCtx, n)
\tif err != nil {
\t\treturn err
\t}
\tdefer stopHomeStack()
\tn.Start(sigCtx)""")
replace(newt / "newt/newt.go", "type Newt struct {", """type Newt struct {
\t// Set before Start; called only after a successful real tunnel probe.
\tOnTunnelProbe func()
""")
ping = newt / "newt/ping.go"
replace(ping, '\t\tlogger.Info("Tunnel connection to server established successfully!")',
        '\t\tif n.OnTunnelProbe != nil { n.OnTunnelProbe() }\n\t\tlogger.Info("Tunnel connection to server established successfully!")', 2)
replace(ping, "\t\t\t\t\trecentLatencies = append(recentLatencies, latency)",
        "\t\t\t\t\tif n.OnTunnelProbe != nil { n.OnTunnelProbe() }\n\t\t\t\t\trecentLatencies = append(recentLatencies, latency)")
