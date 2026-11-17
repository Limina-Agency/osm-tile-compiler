import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.onthegomap.planetiler.geo.GeoUtils;
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

/** Compare valid float polygons with holes against pinned Planetiler snap-and-fix. */
public class HoleSnapOracle {
  private static LinearRing ring(JsonNode coordinates) {
    Coordinate[] points = new Coordinate[coordinates.size()];
    for (int i = 0; i < points.length; i++) {
      points[i] = new Coordinate(coordinates.get(i).get(0).asDouble() / 16,
        coordinates.get(i).get(1).asDouble() / 16);
    }
    return GeoUtils.JTS_FACTORY.createLinearRing(points);
  }

  private static List<double[]> coordinates(CoordinateSequence ring) {
    List<double[]> result = new ArrayList<>();
    for (int i = 0; i < ring.size(); i++) {
      result.add(new double[] {ring.getX(i) * 16, ring.getY(i) * 16});
    }
    return result;
  }

  private static List<Map<String, Object>> polygons(Geometry geometry) {
    List<Map<String, Object>> result = new ArrayList<>();
    for (int i = 0; i < geometry.getNumGeometries(); i++) {
      Polygon polygon = (Polygon) geometry.getGeometryN(i);
      List<List<double[]>> holes = new ArrayList<>();
      for (int h = 0; h < polygon.getNumInteriorRing(); h++) {
        holes.add(coordinates(polygon.getInteriorRingN(h).getCoordinateSequence()));
      }
      result.add(Map.of("exterior", coordinates(polygon.getExteriorRing().getCoordinateSequence()),
        "holes", holes));
    }
    return result;
  }

  public static void main(String[] args) throws Exception {
    JsonNode cases = new ObjectMapper().readTree(Path.of(args[0]).toFile()).get("cases");
    List<Map<String, Object>> results = new ArrayList<>();
    for (JsonNode fixture : cases) {
      LinearRing exterior = ring(fixture.get("exterior"));
      LinearRing[] holes = new LinearRing[fixture.get("holes").size()];
      for (int i = 0; i < holes.length; i++) {
        holes[i] = ring(fixture.get("holes").get(i));
      }
      Polygon input = GeoUtils.JTS_FACTORY.createPolygon(exterior, holes);
      Geometry pointwise = GeometryPrecisionReducer.reducePointwise(input, GeoUtils.TILE_PRECISION);
      Geometry snapped = GeoUtils.snapAndFixPolygon(input, Stats.inMemory(), "oracle");
      Map<String, Object> result = new LinkedHashMap<>();
      result.put("name", fixture.get("name").asText());
      result.put("input_valid", input.isValid());
      result.put("input_area_mvt", input.getArea() * 256);
      result.put("pointwise_valid", pointwise.isValid());
      result.put("pointwise_rings_mvt", polygons(pointwise));
      result.put("output_valid", snapped.isValid());
      result.put("output_area_mvt", snapped.getArea() * 256);
      result.put("output_rings_mvt", polygons(snapped));
      results.add(result);
    }
    System.out.println(new ObjectMapper().writeValueAsString(results));
  }
}
