import osm_tile_compiler as otc
import pytest

from conftest import MONACO

# A new café in Monaco, far above any id the extract uses.
CHANGE = """<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6" generator="osm-tile-compiler tests">
  <create>
    <node id="99999999999" version="1" timestamp="2026-11-01T00:00:00Z"
          changeset="1" lat="43.7384" lon="7.4246">
      <tag k="amenity" v="cafe"/>
      <tag k="name" v="Python test café"/>
    </node>
  </create>
</osmChange>
"""


def test_update_applies_change_files(tmp_path):
    change = tmp_path / "change.osc"
    change.write_text(CHANGE, encoding="utf-8")
    output = tmp_path / "monaco-updated.osm.pbf"
    reports = []
    report = otc.update(MONACO, output, change_files=[change], threads=2, progress=reports.append)
    assert output.is_file()
    assert report["created"] == 1
    assert report["changes_kept"] == 1
    assert report["output_bytes"] == output.stat().st_size
    assert [r.stage for r in reports] == ["scan", "read_changes", "apply"]
    assert reports[1].fraction == 1.0

    # The updated extract compiles.
    result = otc.compile(
        output,
        tmp_path / "updated.pmtiles",
        tmp_path / "sources",
        tmp_path / "temp",
        omit_sources=["all"],
        max_zoom=14,
        threads=2,
    )
    assert result["result"]["tiles"] > 0


def test_update_clips_changes_outside_a_box(tmp_path):
    change = tmp_path / "change.osc"
    change.write_text(CHANGE, encoding="utf-8")
    report = otc.update(
        MONACO,
        tmp_path / "out.osm.pbf",
        change_files=[change],
        clip=(0.0, 0.0, 1.0, 1.0),
    )
    assert report["changes_kept"] == 0


def test_update_needs_changes(tmp_path):
    with pytest.raises(otc.InvalidRequest, match="no change files"):
        otc.update(MONACO, tmp_path / "out.osm.pbf")


@pytest.mark.parametrize(
    "options, message",
    [
        ({"replication": "weekly"}, "weekly"),
        ({"change_files": ["/nowhere/change.osc"]}, "does not exist"),
        ({"change_files": ["/nowhere/change.osc"], "clip": "sometimes"}, "sometimes"),
        ({"change_files": ["/nowhere/change.osc"], "clip": 5}, "clip must be"),
    ],
)
def test_bad_update_requests(tmp_path, options, message):
    with pytest.raises(otc.InvalidRequest, match=message):
        otc.update(MONACO, tmp_path / "out.osm.pbf", **options)


def test_update_can_be_cancelled(tmp_path):
    change = tmp_path / "change.osc"
    change.write_text(CHANGE, encoding="utf-8")
    token = otc.CancelToken()
    token.cancel()
    with pytest.raises(otc.Cancelled):
        otc.update(MONACO, tmp_path / "out.osm.pbf", change_files=[change], cancel=token)
    assert not (tmp_path / "out.osm.pbf").exists()


def test_module_constants():
    assert "transportation" in otc.LAYERS
    assert "en" in otc.LANGUAGES
    assert otc.OMT_SCHEMA_VERSION
    assert otc.__version__
    assert list(otc.COMPILE_STAGES)[0] == "fetch_sources"
