package com.onthegomap.planetiler.render;

import com.carrotsearch.hppc.LongArrayList;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.onthegomap.planetiler.geo.DouglasPeuckerSimplifier;
import com.onthegomap.planetiler.geo.GeoUtils;
import com.onthegomap.planetiler.geo.TileCoord;
import com.onthegomap.planetiler.geo.TileExtents;
import com.onthegomap.planetiler.reader.osm.OsmMultipolygon;
import com.onthegomap.planetiler.stats.Stats;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.CoordinateSequence;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.Polygon;

/** Replays source node-ID ordered members through Planetiler's actual multipolygon builder. */
public class RelationAssemblyOracle {
  private static List<double[]> points(CoordinateSequence sequence) {
    List<double[]> result = new ArrayList<>(sequence.size());
    for (int i = 0; i < sequence.size(); i++) {
      result.add(new double[] {sequence.getX(i) * 16, sequence.getY(i) * 16});
    }
    return result;
  }

  private static List<Map<String, Object>> polygons(Geometry geometry) {
    List<Map<String, Object>> result = new ArrayList<>();
    for (int i = 0; i < geometry.getNumGeometries(); i++) {
      Polygon polygon = (Polygon) geometry.getGeometryN(i);
      List<List<double[]>> holes = new ArrayList<>();
      for (int h = 0; h < polygon.getNumInteriorRing(); h++) {
        holes.add(points(polygon.getInteriorRingN(h).getCoordinateSequence()));
      }
      result.add(Map.of("exterior", points(polygon.getExteriorRing().getCoordinateSequence()),
        "holes", holes));
    }
    return result;
  }

  public static void main(String[] args) throws Exception {
    JsonNode relations = new ObjectMapper().readTree(Path.of(args[0]).toFile());
    List<Map<String, Object>> output = new ArrayList<>();
    for (JsonNode relation : relations) {
      int zoom = relation.get("tile").get(0).asInt();
      int tileX = relation.get("tile").get(1).asInt();
      int tileY = relation.get("tile").get(2).asInt();
      double tolerance = zoom <= 10 ? 0.25 : zoom >= 14 ? 0.0625 : 0.1;
      double minSize = zoom <= 9 ? 2 : zoom == 10 ? 4 : zoom >= 14 ? 0.0625 : 8;
      List<LongArrayList> ways = new ArrayList<>();
      Map<Long, Coordinate> nodes = new HashMap<>();
      for (JsonNode member : relation.get("members")) {
        if (!member.get("type").asText().equals("w")) {
          continue;
        }
        JsonNode way = member.get("way");
        if (way == null || way.isNull()) {
          continue;
        }
        JsonNode ids = way.get("node_ids");
        JsonNode coordinates = way.get("coordinates_lonlat");
        LongArrayList idsInWay = new LongArrayList(ids.size());
        for (int i = 0; i < ids.size(); i++) {
          long id = ids.get(i).asLong();
          idsInWay.add(id);
          double lon = coordinates.get(i).get(0).asDouble();
          double lat = coordinates.get(i).get(1).asDouble();
          long packed = GeoUtils.encodeFlatLocation(lon, lat);
          nodes.putIfAbsent(id, new Coordinate(GeoUtils.decodeWorldX(packed),
            GeoUtils.decodeWorldY(packed)));
        }
        ways.add(idsInWay);
      }
      Geometry assembled = OsmMultipolygon.build(ways, nodes::get,
        relation.get("relation_id").asLong());
      // FeatureRenderer scales world geometry to the zoom's tile-pixel coordinate space.
      Geometry scaled = assembled.copy();
      scaled.apply(new org.locationtech.jts.geom.CoordinateSequenceFilter() {
        @Override public void filter(CoordinateSequence sequence, int i) {
          sequence.setOrdinate(i, 0, sequence.getX(i) * (1 << zoom));
          sequence.setOrdinate(i, 1, sequence.getY(i) * (1 << zoom));
        }
        @Override public boolean isDone() { return false; }
        @Override public boolean isGeometryChanged() { return true; }
      });
      Geometry simplified = DouglasPeuckerSimplifier.simplify(scaled, tolerance / 256);
      var groups = GeometryCoordinateSequences.extractGroups(simplified,
        Math.pow(minSize / 256, 2));
      var extents = new TileExtents.ForZoom(zoom, 0, 0, 1 << zoom, 1 << zoom, null);
      var sliced = TiledGeometry.sliceIntoTiles(groups, 4.0 / 256, true, zoom, extents);
      var clipped = sliced.getTileData().get(TileCoord.ofXYZ(tileX, tileY, zoom));
      if (clipped == null) {
        continue;
      }
      Geometry floating = GeometryCoordinateSequences.reassemblePolygons(clipped);
      Geometry snapped = GeoUtils.snapAndFixPolygon(floating, Stats.inMemory(), "oracle").reverse();
      Map<String, Object> result = new LinkedHashMap<>();
      result.put("relation_id", relation.get("relation_id").asLong());
      result.put("tile", List.of(zoom, tileX, tileY));
      result.put("member_ways", ways.size());
      result.put("assembled_valid", assembled.isValid());
      result.put("assembled_components", assembled.getNumGeometries());
      result.put("assembled_area_world", assembled.getArea());
      List<Map<String, Object>> componentSummary = new ArrayList<>();
      for (int i = 0; i < assembled.getNumGeometries(); i++) {
        Polygon polygon = (Polygon) assembled.getGeometryN(i);
        componentSummary.add(Map.of("area_world", polygon.getArea(),
          "exterior_points", polygon.getExteriorRing().getNumPoints(),
          "holes", polygon.getNumInteriorRing()));
      }
      result.put("assembled_component_summary", componentSummary);
      result.put("simplified_valid", simplified.isValid());
      result.put("float_valid", floating.isValid());
      result.put("float_rings_mvt", polygons(floating));
      result.put("snapped_rings_mvt", polygons(snapped));
      result.put("snapped_area_mvt2", snapped.getArea() * 256);
      output.add(result);
    }
    System.out.println(new ObjectMapper().writeValueAsString(output));
  }
}
