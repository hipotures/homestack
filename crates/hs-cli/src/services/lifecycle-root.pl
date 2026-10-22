# Trusted HomeStack code, never supplied by a service manifest. All runtime
# values arrive on stdin over the existing private root SSH transport.
# Only Proxmox's storage, OCI extraction and config-locking primitives are used.
use strict;
use warnings;
use JSON::PP qw(decode_json);
use PVE::LXC;
use PVE::LXC::Config;
use PVE::LXC::Create;
use PVE::INotify;
use PVE::Storage;
use PVE::Tools;
my $r = decode_json(do { local $/; <STDIN> });
my $id = $r->{ctid};
die "invalid CTID\n" unless $id =~ /^\d+$/ && $id > 0;
die "wrong node\n" unless PVE::INotify::nodename() eq $r->{node};
my $store = PVE::Storage::config();
my $home = $r->{home};
my (undef, undef, $owner) = PVE::Storage::parse_volname($store, $home);
die "foreign home volume\n" unless defined($owner) && $owner == $id;
# A node-local helper cannot authorize shared-volume destruction. Rust's
# inspection likewise refuses shared home storage for destructive operations.
my ($home_store) = PVE::Storage::parse_volume_id($home);
my $scfg = PVE::Storage::storage_check_enabled($store, $home_store);
die "shared home storage requires review\n" if $scfg->{shared};
my $assert_unreferenced = sub {
    for my $file (glob('/etc/pve/nodes/*/lxc/*.conf'), glob('/etc/pve/nodes/*/qemu-server/*.conf')) {
        open(my $fh, '<', $file) or die "cannot inspect volume references\n";
        local $/;
        my $text = <$fh>;
        close($fh);
        die "home volume is still referenced\n" if $text =~ /\Q$home\E(?:,|\s|$)/;
    }
};
if ($r->{action} eq 'verify-runtime') {
    my $conf = PVE::LXC::Config->load_config($id);
    die "home changed\n" unless ($conf->{mp0} // '') eq $r->{mp0};
    my $devices = '';
    PVE::Tools::run_command(['pct', 'exec', $id, '--', 'stat', '-c', '%d', '/', '/home/app'], outfunc => sub { $devices .= "$_[0]\n"; });
    my @devices = grep { /^\d+$/ } split(/\n/, $devices);
    die "persistent home is not observed as a separate filesystem\n" unless @devices == 2 && $devices[0] ne $devices[1];
    my $usage = '';
    PVE::Tools::run_command(['pct', 'exec', $id, '--', 'df', '-Pk', '/', '/home/app'], outfunc => sub { $usage .= "$_[0]\n"; });
    print $usage;
    exit;
}
if ($r->{action} eq 'assert-home-free') {
    $assert_unreferenced->();
    print "retained home is unreferenced\n";
    exit;
}
if ($r->{action} eq 'verify-image') {
    my $file = PVE::Storage::abs_filesystem_path($store, $r->{archive});
    my $hash = '';
    PVE::Tools::run_command([['skopeo', 'inspect', '--raw', "oci-archive:$file"], ['sha256sum']], outfunc => sub { $hash .= $_[0]; });
    my $expected = $r->{platform};
    $expected =~ s/^sha256://;
    die "archive digest differs from review\n" unless $hash =~ /^\Q$expected\E(?:\s|$)/;
    print "archive digest verified\n";
    exit;
}
if ($r->{action} eq 'delete-data') {
    die "CTID still exists\n" if -e PVE::LXC::Config->config_file($id);
    $assert_unreferenced->();
    PVE::Storage::vdisk_free($store, $home);
    print "data removed\n";
    exit;
}
PVE::LXC::Config->lock_config($id, sub {
    my $conf = PVE::LXC::Config->load_config($id);
    PVE::LXC::Config->check_lock($conf);
    die "template containers require review\n" if $conf->{template};
    require PVE::HA::Config;
    die "HA-managed container requires review\n" if PVE::HA::Config::service_is_configured("ct:$id");
    PVE::LXC::Config->check_protection($conf, 'service lifecycle');
    die "configuration changed\n" unless $conf->{digest} eq $r->{digest};
    die "home changed\n" unless ($conf->{mp0} // '') eq $r->{mp0};
    die "root changed\n" unless ($conf->{rootfs} // '') eq $r->{rootfs};
    die "container is running\n" if PVE::LXC::check_running($id);
    die "pending configuration\n" if keys %{$conf->{pending} // {}};
    die "snapshots exist\n" if keys %{$conf->{snapshots} // {}};
    my %runtime_keys = map { $_ => 1 } qw(lxc.init.uid lxc.init.gid lxc.init.groups lxc.init.cwd lxc.signal.halt);
    for my $entry (@{$conf->{lxc} // []}) {
        die "custom LXC configuration requires review\n" unless $runtime_keys{$entry->[0]};
    }
    die "not unprivileged\n" unless $conf->{unprivileged};
    for my $key (keys %$conf) {
        die "unexpected mount point\n" if $key =~ /^mp[1-9]\d*$/;
    }
    if ($r->{action} eq 'uninstall') {
        # Unlike API deletion of mp0, forgetting this single verified reference
        # does not create unusedN. Refuse unrelated disks before CT removal.
        die "unused disks require review\n" if grep { /^unused\d+$/ } keys %$conf;
        delete $conf->{mp0};
        PVE::LXC::Config->write_config($id, $conf);
        print "home retained\n";
        return;
    }
    die "unknown action\n";
});
