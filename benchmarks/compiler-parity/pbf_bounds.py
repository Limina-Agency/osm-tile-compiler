"""Print an OSM PBF's header bounding box as 'WEST SOUTH EAST NORTH'.

Standard library only, so it runs before the benchmark environment exists.
"""
import struct
import sys
import zlib


def varint(data, at):
    value = shift = 0
    while True:
        byte = data[at]
        at += 1
        value |= (byte & 0x7F) << shift
        shift += 7
        if byte < 0x80:
            return value, at


def fields(data):
    at = 0
    while at < len(data):
        key, at = varint(data, at)
        number, wire = key >> 3, key & 7
        if wire == 0:
            value, at = varint(data, at)
        elif wire == 2:
            length, at = varint(data, at)
            value, at = data[at:at + length], at + length
        else:
            raise ValueError('unexpected wire type %d in the PBF header' % wire)
        yield number, value


def bounds(path):
    with open(path, 'rb') as stream:
        (length,) = struct.unpack('>I', stream.read(4))
        blob_header = dict(fields(stream.read(length)))
        if blob_header.get(1) != b'OSMHeader':
            raise ValueError('the first block is not an OSMHeader')
        blob = dict(fields(stream.read(blob_header[3])))
    data = zlib.decompress(blob[3]) if 3 in blob else blob[1]
    box = dict(fields(dict(fields(data)).get(1, b'')))
    if not all(k in box for k in (1, 2, 3, 4)):
        raise ValueError('the PBF header has no bounding box')

    def degrees(value):
        return ((value >> 1) ^ -(value & 1)) / 1e9

    left, right, top, bottom = (degrees(box[k]) for k in (1, 2, 3, 4))
    return left, bottom, right, top


if __name__ == '__main__':
    print(*bounds(sys.argv[1]))
