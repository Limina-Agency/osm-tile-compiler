import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('pbf_bounds', Path(__file__).with_name('pbf_bounds.py'))
pbf_bounds = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pbf_bounds)

MONACO = Path(__file__).resolve().parents[2] / 'fixtures' / 'monaco.osm.pbf'


class PbfBoundsTests(unittest.TestCase):
    def test_reads_the_header_bounding_box(self):
        # The same box the compiler reads (fixtures/monaco-features.json).
        self.assertEqual(pbf_bounds.bounds(MONACO), (7.409205, 43.72335, 7.448637, 43.75169))

    def test_decodes_zigzag_varints(self):
        self.assertEqual(list(pbf_bounds.fields(bytes([0x08, 0x96, 0x01]))), [(1, 150)])


if __name__ == '__main__':
    unittest.main()
