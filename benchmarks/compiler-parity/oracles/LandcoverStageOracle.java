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
import org.locationtech.jts.geom.Polygon;

/** Pinned renderer stage oracle; defaults retain the original way 748863343 fixture. */
public class LandcoverStageOracle {
  private static List<double[]> points(CoordinateSequence sequence, double scale) {
    List<double[]> result = new ArrayList<>(sequence.size());
    for (int i = 0; i < sequence.size(); i++) {
      result.add(new double[] {sequence.getX(i) * scale, sequence.getY(i) * scale});
    }
    return result;
  }

  private static List<Map<String, Object>> polygons(Geometry geometry) {
    List<Map<String, Object>> result = new ArrayList<>();
    if (geometry.isEmpty()) return result;
    for (int i = 0; i < geometry.getNumGeometries(); i++) {
      Polygon polygon = (Polygon) geometry.getGeometryN(i);
      List<List<double[]>> holes = new ArrayList<>();
      for (int h = 0; h < polygon.getNumInteriorRing(); h++) {
        holes.add(points(polygon.getInteriorRingN(h).getCoordinateSequence(), 16));
      }
      result.add(Map.of(
        "exterior", points(polygon.getExteriorRing().getCoordinateSequence(), 16),
        "holes", holes
      ));
    }
    return result;
  }

  public static void main(String[] args) throws Exception {
    JsonNode fixture = new ObjectMapper().readTree(Path.of(args[0]).toFile());
    int zoom = fixture.path("zoom").asInt(9);
    int x = fixture.path("x").asInt(284);
    int y = fixture.path("y").asInt(173);
    double tolerance = fixture.path("tolerance_pixels").asDouble(0.25);
    double minSize = fixture.path("min_size_pixels").asDouble(2.0);
    int scale = 1 << zoom;
    JsonNode coordinates = fixture.get("coordinates_lonlat");
    Coordinate[] world = new Coordinate[coordinates.size()];
    for (int i = 0; i < world.length; i++) {
      double lon = coordinates.get(i).get(0).asDouble();
      double lat = coordinates.get(i).get(1).asDouble();
      long packed = GeoUtils.encodeFlatLocation(lon, lat);
      world[i] = new Coordinate(GeoUtils.decodeWorldX(packed) * scale,
        GeoUtils.decodeWorldY(packed) * scale);
    }
    Polygon scaled = GeoUtils.JTS_FACTORY.createPolygon(world);
    Geometry simplified = DouglasPeuckerSimplifier.simplify(scaled, tolerance / 256);
    var groups = GeometryCoordinateSequences.extractGroups(simplified, Math.pow(minSize / 256, 2));
    var extents = new TileExtents.ForZoom(zoom, 0, 0, scale, scale, null);
    var sliced = TiledGeometry.sliceIntoTiles(groups, 4.0 / 256, true, zoom, extents);
    var clipped = sliced.getTileData().get(TileCoord.ofXYZ(x, y, zoom));
    Geometry unsnapped = clipped == null ? GeoUtils.JTS_FACTORY.createPolygon() :
      GeometryCoordinateSequences.reassemblePolygons(clipped);
    Geometry snapped = GeoUtils.snapAndFixPolygon(unsnapped, Stats.inMemory(), "oracle").reverse();

    Map<String, Object> output = new LinkedHashMap<>();
    output.put("raw_count", world.length);
    output.put("simplified_count", simplified.getNumPoints());
    output.put("clipped_group_count", clipped == null ? 0 : clipped.size());
    output.put("clipped_float_rings_mvt", polygons(unsnapped));
    output.put("snapped_rings_mvt", polygons(snapped));
    output.put("snapped_area_mvt", snapped.getArea() * 256);
    System.out.println(new ObjectMapper().writeValueAsString(output));
  }
}
