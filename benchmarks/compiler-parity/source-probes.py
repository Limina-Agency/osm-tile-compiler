"""Derive deterministic named OSM point probes from the exact benchmark PBF."""
import argparse
import json
from pathlib import Path
import osmium


class Probes(osmium.SimpleHandler):
    def __init__(self):
        super().__init__()
        self.picks = {}
        self.counts = {}

    def node(self, node):
        if not node.tags.get('name') or not node.location.valid():
            return
        tags = dict(node.tags)
        if tags.get('place') in ('city', 'town', 'village'):
            category = 'place:' + tags['place']
            layers = {'native': ['places'], 'planetiler': ['place']}
        elif tags.get('natural') == 'peak':
            category = 'natural:peak'
            layers = {'native': ['pois'], 'planetiler': ['mountain_peak']}
        else:
            return
        self.counts[category] = self.counts.get(category, 0) + 1
        # Geographic bins, deterministic lowest ID per category/bin.
        key = (category, int(node.location.lon * 2), int(node.location.lat * 2))
        if key not in self.picks or node.id < self.picks[key]['osm_id']:
            self.picks[key] = dict(osm_type='node', osm_id=node.id,
                name=tags['name'], category=category, lon=node.location.lon,
                lat=node.location.lat, tags=tags, layers=layers)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('pbf')
    p.add_argument('--output', required=True)
    a = p.parse_args()
    h = Probes()
    # Filter untagged geometry nodes in libosmium before Python callbacks.
    for node in osmium.FileProcessor(a.pbf, entities=osmium.osm.NODE).with_filter(osmium.filter.KeyFilter('name')):
        h.node(node)
    result = dict(source=str(Path(a.pbf).resolve()), candidates=h.counts,
                  selection='lowest OSM node ID per category and 0.5 degree bin',
                  probes=sorted(h.picks.values(), key=lambda p: (p['category'], p['osm_id'])))
    Path(a.output).write_text(json.dumps(result, indent=2, ensure_ascii=False), encoding='utf-8')
    print(f"Wrote {len(result['probes'])} source probes")


if __name__ == '__main__':
    main()
