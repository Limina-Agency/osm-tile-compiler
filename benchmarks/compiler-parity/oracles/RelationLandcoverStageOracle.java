package com.onthegomap.planetiler.render;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.onthegomap.planetiler.geo.DouglasPeuckerSimplifier;
import com.onthegomap.planetiler.geo.GeoUtils;
import com.onthegomap.planetiler.geo.TileCoord;
import com.onthegomap.planetiler.geo.TileExtents;
import com.onthegomap.planetiler.stats.Stats;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.CoordinateSequence;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.LinearRing;
import org.locationtech.jts.geom.Polygon;
import org.locationtech.jts.precision.GeometryPrecisionReducer;

/** Source-way replay for small landcover multipolygon relations. */
public class RelationLandcoverStageOracle {
  private static LinearRing ring(JsonNode coordinates, int scale) {
    Coordinate[] points = new Coordinate[coordinates.size()];
    for (int i = 0; i < points.length; i++) {
      double lon = coordinates.get(i).get(0).asDouble();
      double lat = coordinates.get(i).get(1).asDouble();
      long packed = GeoUtils.encodeFlatLocation(lon, lat);
      points[i] = new Coordinate(GeoUtils.decodeWorldX(packed) * scale,
        GeoUtils.decodeWorldY(packed) * scale);
    }
    return GeoUtils.JTS_FACTORY.createLinearRing(points);
  }

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
    JsonNode input = new ObjectMapper().readTree(Path.of(args[0]).toFile());
    // Accept the focused checked-in fixture or a collection of extracted relations.
    Iterable<JsonNode> relations = input.isArray() ? input : List.of(input.get("source"));
    List<Map<String, Object>> output = new ArrayList<>();
    for (JsonNode relation : relations) {
      int zoom = relation.get("tile").get(0).asInt();
      int tileX = relation.get("tile").get(1).asInt();
      int tileY = relation.get("tile").get(2).asInt();
      double tolerance = zoom <= 10 ? 0.25 : zoom >= 14 ? 0.0625 : 0.1;
      double minSize = zoom <= 9 ? 2 : zoom == 10 ? 4 : zoom >= 14 ? 0.0625 : 8;
      List<LinearRing> outers = new ArrayList<>();
      List<LinearRing> inners = new ArrayList<>();
      for (JsonNode member : relation.get("members")) {
        LinearRing ring = ring(member.get("way").get("coordinates_lonlat"), 1 << zoom);
        (member.get("role").asText().equals("outer") ? outers : inners).add(ring);
      }
      if (outers.size() != 1) {
        continue;
      }
      Polygon source = GeoUtils.JTS_FACTORY.createPolygon(outers.getFirst(), inners.toArray(LinearRing[]::new));
      Geometry simplified = DouglasPeuckerSimplifier.simplify(source, tolerance / 256);
      var groups = GeometryCoordinateSequences.extractGroups(simplified, Math.pow(minSize / 256, 2));
      var extents = new TileExtents.ForZoom(zoom, 0, 0, 1 << zoom, 1 << zoom, null);
      var sliced = TiledGeometry.sliceIntoTiles(groups, 4.0 / 256, true, zoom, extents);
      var clipped = sliced.getTileData().get(TileCoord.ofXYZ(tileX, tileY, zoom));
      if (clipped == null) {
        continue;
      }
      Geometry floating = GeometryCoordinateSequences.reassemblePolygons(clipped);
      Geometry pointwise = GeometryPrecisionReducer.reducePointwise(floating, GeoUtils.TILE_PRECISION);
      Geometry snapped = GeoUtils.snapAndFixPolygon(floating, Stats.inMemory(), "oracle").reverse();
      Map<String, Object> result = new LinkedHashMap<>();
      result.put("relation_id", relation.get("relation_id").asLong());
      result.put("tile", List.of(zoom, tileX, tileY));
      result.put("source_valid", source.isValid());
      result.put("simplified_valid", simplified.isValid());
      result.put("float_valid", floating.isValid());
      result.put("pointwise_valid", pointwise.isValid());
      result.put("source_points", source.getNumPoints());
      result.put("simplified_points", simplified.getNumPoints());
      result.put("float_rings_mvt", polygons(floating));
      result.put("snapped_rings_mvt", polygons(snapped));
      result.put("snapped_area_mvt", snapped.getArea() * 256);
      output.add(result);
    }
    System.out.println(new ObjectMapper().writeValueAsString(output));
  }
}
