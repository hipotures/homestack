"""Read-only guest inspection used by the online resize workflow."""

# Keep this as a standalone script. The desktop sends it to the guest with
# python3 -c and imports only this string from the package.
INSPECT = r'''
import json, os, re, shutil, stat, subprocess, sys
from decimal import Decimal, InvalidOperation

mount, role, label = sys.argv[1:]

def fail(message):
    sys.exit(str(message))

def require(condition, message):
    if not condition:
        fail(message)

def command(*args):
    try:
        return subprocess.check_output(args, text=True).strip()
    except (OSError, subprocess.CalledProcessError) as exc:
        fail('Guest command failed: ' + str(args[0]) + ': ' + str(exc))

def optional_command(*args):
    try:
        return subprocess.check_output(args, text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        return ''

def json_command(*args):
    try:
        return json.loads(command(*args))
    except (ValueError, TypeError) as exc:
        fail('Invalid output from ' + str(args[0]) + ': ' + str(exc))

def tool(name):
    require(shutil.which(name), 'Missing guest tool: ' + name)

for _tool in ('findmnt', 'lsblk', 'blkid', 'blockdev'):
    tool(_tool)

filesystems = json_command(
    'findmnt', '--json', '--mountpoint', mount,
    '-o', 'SOURCE,FSTYPE,TARGET,OPTIONS,MAJ:MIN',
).get('filesystems', [])
require(len(filesystems) == 1, 'Cannot identify exact mounted filesystem: ' + mount)
fs = filesystems[0]
filesystem = str(fs.get('fstype') or '')
require(fs.get('target') == mount and filesystem in ('ext4', 'btrfs'),
        'Resize requires an exact ext4 or btrfs mount: ' + mount)
require('rw' in str(fs.get('options', '')).split(','), 'Filesystem is read-only')
fs_mm = str(fs.get('maj:min') or '')
tool('resize2fs' if filesystem == 'ext4' else 'btrfs')

raw_rows = json_command(
    'lsblk', '--json', '--bytes', '--paths', '--list',
    '-o', 'NAME,TYPE,PKNAME,SERIAL,SIZE,MAJ:MIN',
).get('blockdevices', [])

def flatten(rows):
    result = []
    for row in rows:
        result.append(row)
        result.extend(flatten(row.get('children') or []))
    return result

rows = []
for _row in flatten(raw_rows):
    _name = _row.get('name')
    if not _name:
        continue
    if not str(_name).startswith('/'):
        _name = '/dev/' + str(_name)
    rows.append(dict(_row, name=_name))
by_mm = {}
by_path = {}
for _row in rows:
    _mm = str(_row.get('maj:min') or '')
    if _mm:
        require(_mm not in by_mm, 'Ambiguous lsblk MAJ:MIN: ' + _mm)
        by_mm[_mm] = _row
    by_path[os.path.realpath(_row['name'])] = _row
    by_path[os.path.realpath('/dev/' + os.path.basename(_row['name']))] = _row

def row_for(value):
    if not value:
        return None
    value = str(value)
    if not value.startswith('/'):
        value = '/dev/' + value
    return by_path.get(os.path.realpath(value))

def sysfs_value(path):
    try:
        with open(path) as handle:
            return handle.read().strip()
    except (OSError, IOError):
        return ''

def dm_name(row):
    real = os.path.realpath(row['name'])
    base = os.path.basename(real)
    if not base.startswith('dm-'):
        return ''
    return sysfs_value('/sys/class/block/' + base + '/dm/name')

def row_key(row):
    return str(row.get('maj:min') or os.path.realpath(row['name']))

if filesystem == 'ext4':
    btrfs_devid = None
    require(fs_mm, 'Mounted filesystem has no MAJ:MIN: ' + str(fs.get('source', '')))
    matches = [row for row in rows if str(row.get('maj:min') or '') == fs_mm]
    require(len(matches) == 1,
            'Cannot identify mounted block device: ' + str(fs.get('source', '')) + ' (' + fs_mm + ')')
    entry = matches[0]
else:
    btrfs_show = command('btrfs', 'filesystem', 'show', '--raw', mount)
    total = re.search(r'(?m)Total devices\s+(\d+)', btrfs_show)
    require(total and int(total.group(1)) == 1,
            'Resize requires a single-device btrfs filesystem')
    devices = [match.groups() for match in re.finditer(
        r'(?m)^\s*devid\s+(\d+).*?\bpath\s+(.+?)\s*$', btrfs_show)]
    require(len(devices) == 1 and devices[0][1] not in ('unknown', '-'),
            'Cannot identify the btrfs backing device')
    btrfs_devid = int(devices[0][0])
    paths = [devices[0][1].strip()]
    entry = None
    source = str(fs.get('source') or '').split('[', 1)[0]
    for candidate in (paths[0], source):
        candidate_row = row_for(candidate) if candidate else None
        if candidate_row is not None:
            if entry is None:
                entry = candidate_row
            else:
                require(row_key(entry) == row_key(candidate_row),
                        'Mounted btrfs device does not match filesystem info')
    require(entry is not None, 'Cannot identify the mounted btrfs block device')

def parents(row):
    found = {}
    candidate = row_for(row.get('pkname'))
    if candidate is not None:
        found[row_key(candidate)] = candidate
    real = os.path.realpath(row['name'])
    base = os.path.basename(real)
    try:
        slaves = os.listdir('/sys/class/block/' + base + '/slaves')
    except (OSError, IOError):
        slaves = []
    for slave in slaves:
        candidate = row_for(slave)
        if candidate is not None:
            found[row_key(candidate)] = candidate
    return list(found.values())

def walk_chain(start):
    chain = [start]
    seen = {row_key(start)}
    current = start
    while current.get('type') not in ('disk', 'part'):
        current_parents = parents(current)
        require(current_parents, 'Cannot identify parent of ' + current['name'])
        require(len(current_parents) == 1,
                'Ambiguous multiple parents for ' + current['name'])
        current = current_parents[0]
        require(row_key(current) not in seen, 'Cyclic block-device parent chain')
        seen.add(row_key(current))
        chain.append(current)
    return chain

chain = walk_chain(entry)
types = [str(row.get('type') or '') for row in chain]
require(all(kind in ('disk', 'part', 'crypt', 'lvm') for kind in types),
        'Unsupported block-device layout: ' + ' -> '.join(types))
require(types.count('crypt') <= 1 and types.count('lvm') <= 1,
        'Nested encrypted or LVM layouts are not supported')
if 'crypt' in types and 'lvm' in types:
    require(types.index('lvm') < types.index('crypt'),
            'Unsupported LVM/encryption nesting')

physical = chain[-1]
require(physical.get('type') in ('disk', 'part'),
        'Unsupported mounted block device: ' + entry['name'])
if physical.get('type') == 'part':
    partition_parent = row_for(physical.get('pkname'))
    require(partition_parent is not None and partition_parent.get('type') == 'disk',
            'Unsupported partition parent')
    parent = partition_parent
else:
    parent = physical

disks = [row for row in rows if row.get('type') == 'disk'
         and not os.path.basename(row['name']).startswith('zram')]
require(len(disks) == 2, 'Resize requires exactly the root and home data disks')
homes = [row for row in disks if str(row.get('serial') or '').strip() == label]
require(len(homes) == 1, 'Cannot identify the persistent home disk by serial')
require(row_key(parent) == row_key(homes[0]) if role == 'home'
        else row_key(parent) != row_key(homes[0]),
        'Mounted filesystem does not match the selected workspace disk')
if role == 'home':
    require(command('blkid', '-s', 'LABEL', '-o', 'value', entry['name']) == label,
            'Persistent home label mismatch')

partition_device = None
partition = None
if physical.get('type') == 'part':
    tool('growpart')
    base = os.path.basename(os.path.realpath(physical['name']))
    try:
        partition = int(sysfs_value('/sys/class/block/' + base + '/partition'))
        start = int(sysfs_value('/sys/class/block/' + base + '/start'))
    except ValueError:
        fail('Cannot inspect partition geometry: ' + physical['name'])
    require(partition > 0, 'Invalid partition number: ' + physical['name'])
    partition_device = physical['name']
    for candidate in rows:
        if candidate.get('type') != 'part':
            continue
        candidate_parent = row_for(candidate.get('pkname'))
        if candidate_parent is None or row_key(candidate_parent) != row_key(parent):
            continue
        candidate_base = os.path.basename(os.path.realpath(candidate['name']))
        try:
            candidate_start = int(sysfs_value(
                '/sys/class/block/' + candidate_base + '/start'))
        except ValueError:
            fail('Cannot inspect partition geometry: ' + candidate['name'])
        require(candidate_start <= start,
                'Selected partition is not the last partition on disk')

def decimal_int(value, description):
    try:
        number = Decimal(str(value))
        require(number == number.to_integral_value(), 'Non-integer ' + description)
        return int(number)
    except (InvalidOperation, ValueError, TypeError):
        fail('Invalid ' + description + ': ' + str(value))

def report_rows(value, key):
    reports = value.get('report', []) if isinstance(value, dict) else []
    result = []
    for report in reports:
        result.extend(report.get(key, []) or [])
    return result

crypt = None
lvm = None

def same_block(left, right):
    if not left or not right:
        return False
    left_row, right_row = row_for(left), row_for(right)
    if left_row is not None and right_row is not None:
        return row_key(left_row) == row_key(right_row)
    return os.path.realpath(str(left)) == os.path.realpath(str(right))

def crypttab_tokens(line):
    line = line.split('#', 1)[0].strip()
    if not line:
        return []
    require(not re.search(r'\\(?![0-7]{3})', line),
            'Unsupported escape in /etc/crypttab')
    fields = line.split()
    decoded = []
    for field in fields:
        decoded.append(re.sub(r'\\([0-7]{3})',
                              lambda match: chr(int(match.group(1), 8)), field))
    return decoded

def crypttab_rows():
    try:
        with open('/etc/crypttab', encoding='utf-8') as handle:
            lines = handle.readlines()
    except FileNotFoundError:
        return []
    except OSError as exc:
        fail('Cannot read /etc/crypttab: ' + str(exc))
    result = []
    for line in lines:
        fields = crypttab_tokens(line.rstrip('\n'))
        if not fields:
            continue
        require(3 <= len(fields) <= 4, 'Unsupported /etc/crypttab row')
        result.append(fields)
    return result

if 'crypt' in types:
    tool('cryptsetup')
    crypt_row = chain[types.index('crypt')]
    crypt_parents = parents(crypt_row)
    require(len(crypt_parents) == 1, 'Encrypted mapping has ambiguous backing devices')
    backing = crypt_parents[0]['name']
    require(optional_command('blkid', '-s', 'TYPE', '-o', 'value', backing) == 'crypto_LUKS',
            'Encrypted backing device is not LUKS: ' + backing)
    mapper_name = dm_name(crypt_row) or (
        os.path.basename(crypt_row['name']) if '/mapper/' in crypt_row['name']
        else os.path.basename(os.path.realpath(crypt_row['name']))
    )
    crypt_matches = []
    backing_uuid = optional_command('blkid', '-s', 'UUID', '-o', 'value', backing)
    backing_partuuid = optional_command('blkid', '-s', 'PARTUUID', '-o', 'value', backing)
    backing_label = optional_command('blkid', '-s', 'LABEL', '-o', 'value', backing)
    backing_real = os.path.realpath(backing)
    for crypt_fields in crypttab_rows():
        if crypt_fields[0] != mapper_name:
            continue
        source = crypt_fields[1]
        source_match = False
        if source.startswith('UUID='):
            source_match = source[5:] == backing_uuid
        elif source.startswith('PARTUUID='):
            source_match = source[9:] == backing_partuuid
        elif source.startswith('LABEL='):
            source_match = source[6:] == backing_label
        elif source not in ('none', '-'):
            source_match = os.path.realpath(source) == backing_real
        if source_match:
            crypt_matches.append(crypt_fields)
    require(len(crypt_matches) <= 1, 'Ambiguous /etc/crypttab rows for ' + mapper_name)
    key_file = None
    auth = 'passphrase'
    if crypt_matches:
        fields = crypt_matches[0]
        key_value = fields[2]
        options = fields[3].split(',') if len(fields) == 4 and fields[3] else []
        unsupported = [option for option in options
                       if option == 'keyscript' or option.startswith('keyscript=')
                       or option in ('offset', 'skip', 'sector-size', 'keyfile-offset',
                                     'keyfile-size', 'key-slot', 'header')
                       or option.startswith(('offset=', 'skip=', 'sector-size=',
                                             'keyfile-offset=', 'keyfile-size=',
                                             'key-slot=', 'header='))]
        require(not unsupported,
                'Unsupported /etc/crypttab option: ' + unsupported[0] if unsupported else '')
        if key_value not in ('none', '-'):
            require(key_value.startswith('/'),
                    'Crypttab key file must be an absolute path: ' + key_value)
            key_file = key_value
            try:
                key_stat = os.stat(key_file)
            except OSError:
                fail('Crypttab key file is missing: ' + key_file)
            require(stat.S_ISREG(key_stat.st_mode),
                    'Crypttab key file is not a regular file: ' + key_file)
            auth = 'keyfile'
    crypt = dict(name=mapper_name, device=backing, key_file=key_file, auth=auth)

if 'lvm' in types:
    for _tool in ('pvs', 'vgs', 'lvs', 'pvresize', 'lvextend'):
        tool(_tool)
    lv_row = chain[types.index('lvm')]
    lvs_value = json_command(
        'lvs', '--reportformat', 'json', '--units', 'b', '--nosuffix',
        '--noheadings', '-o', 'lv_path,lv_name,vg_name,lv_size,lv_attr,segtype',
    )
    candidates = []
    for candidate in report_rows(lvs_value, 'lv'):
        if same_block(candidate.get('lv_path'), lv_row['name']):
            candidates.append(candidate)
    require(len(candidates) == 1, 'Cannot identify mounted logical volume')
    selected = candidates[0]
    attrs = str(selected.get('lv_attr') or '')
    require(attrs.startswith('-') and str(selected.get('segtype') or '') == 'linear',
            'Only ordinary linear logical volumes are supported')
    vg_name = str(selected.get('vg_name') or '')
    require(vg_name, 'Logical volume has no volume group')
    all_lvs = [candidate for candidate in report_rows(lvs_value, 'lv')
               if str(candidate.get('vg_name') or '') == vg_name]
    require(len(all_lvs) == 1, 'Resize requires a dedicated volume group with one logical volume')
    vgs_value = json_command(
        'vgs', '--reportformat', 'json', '--units', 'b', '--nosuffix',
        '--noheadings', '-o', 'vg_name,vg_extent_size',
    )
    vgs_match = [candidate for candidate in report_rows(vgs_value, 'vg')
                 if str(candidate.get('vg_name') or '') == vg_name]
    require(len(vgs_match) == 1, 'Cannot identify volume group: ' + vg_name)
    pvs_value = json_command(
        'pvs', '--reportformat', 'json', '--units', 'b', '--nosuffix',
        '--noheadings', '-o', 'pv_name,vg_name',
    )
    pvs_match = [candidate for candidate in report_rows(pvs_value, 'pv')
                 if str(candidate.get('vg_name') or '') == vg_name]
    require(len(pvs_match) == 1, 'Resize requires a volume group with one physical volume')
    pv_path = str(pvs_match[0].get('pv_name') or '')
    require(pv_path and any(same_block(pv_path, row['name'])
                             for row in chain[types.index('lvm') + 1:]),
            'Volume group physical volume does not match the mounted disk chain')
    lvm = dict(
        lv_path=str(selected.get('lv_path') or ''),
        pv_path=pv_path,
        lv_size_bytes=decimal_int(selected.get('lv_size'), 'logical volume size'),
        extent_size_bytes=decimal_int(vgs_match[0].get('vg_extent_size'), 'volume group extent size'),
        vg_name=vg_name,
    )

print(json.dumps(dict(
    device=entry['name'], parent=parent['name'], partition=partition,
    size_bytes=decimal_int(parent.get('size'), 'disk size'),
    partition_device=partition_device, crypt=crypt, lvm=lvm,
    filesystem=filesystem, mount=mount, btrfs_devid=btrfs_devid,
)))
'''
