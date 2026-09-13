#!/usr/bin/env python3
"""Print control fields of a Debian binary archive, without dpkg.

    control-fields.py <archive.deb>                  the whole control file
    control-fields.py <archive.deb> Field [Field...]  one value per line, empty when absent

The host carries no dpkg (docs/design/build.md section 0); fetch.sh and
publish.sh read the fields they verify through this instead. A .deb is an
`ar` archive whose `control.tar.*` member holds `./control`; gzip and xz are
read, anything else is refused by name.
"""
import io
import sys
import tarfile


def members(path):
    data = open(path, 'rb').read()
    if data[:8] != b'!<arch>\n':
        raise SystemExit(f'error: {path} is not an ar archive, so not a Debian binary package')
    at = 8
    while at + 60 <= len(data):
        name = data[at:at + 16].decode('ascii', 'replace').strip().rstrip('/')
        size = int(data[at + 48:at + 58].decode('ascii').strip())
        yield name, data[at + 60:at + 60 + size]
        at += 60 + size + (size & 1)


def control(path):
    for name, body in members(path):
        if name.startswith('control.tar'):
            if name.endswith(('.zst', '.lz4', '.bz2')):
                raise SystemExit(f'error: {path} compresses its control archive as {name}; only control.tar, .gz and .xz are read here')
            with tarfile.open(fileobj=io.BytesIO(body), mode='r:*') as tar:
                for member in tar.getmembers():
                    if member.name.lstrip('./') == 'control' and member.isfile():
                        return tar.extractfile(member).read().decode('utf-8')
            raise SystemExit(f'error: {path}: {name} carries no control file')
    raise SystemExit(f'error: {path} carries no control.tar member')


def fields(text):
    result, key = {}, None
    for line in text.splitlines():
        if line.startswith((' ', '\t')):
            if key is not None:
                result[key] += '\n' + line
        elif line:
            key, _, value = line.partition(':')
            result[key.strip()] = value.strip()
    return result


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    text = control(sys.argv[1])
    if len(sys.argv) == 2:
        sys.stdout.write(text)
        return
    parsed = fields(text)
    for name in sys.argv[2:]:
        print(parsed.get(name, ''))


if __name__ == '__main__':
    main()
