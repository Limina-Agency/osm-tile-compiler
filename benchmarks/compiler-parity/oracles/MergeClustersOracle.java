import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.onthegomap.planetiler.FeatureMerge;
import com.onthegomap.planetiler.VectorTile;
import com.onthegomap.planetiler.geo.GeoUtils;
import com.onthegomap.planetiler.stats.Stats;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.LinearRing;
import org.locationtech.jts.geom.Polygon;
import org.locationtech.jts.geom.util.AffineTransformation;

/**
 * Replays each case of omt-merge-jts-clusters.json through the pinned
 * FeatureMerge.mergeNearbyPolygons and compares the result with the case's
 * expected polygons (integer MVT units, 16 per tile pixel).
 */
public class MergeClustersOracle {
  private static LinearRing ring(JsonNode points, double scale) {
    Coordinate[] coordinates = new Coordinate[points.size()];
    for (int i = 0; i < points.size(); i++) {
      var point = points.get(i);
      coordinates[i] = point.isArray()
        ? new Coordinate(point.get(0).asDouble() / scale, point.get(1).asDouble() / scale)
        : new Coordinate(point.get("x").asDouble() / scale, point.get("y").asDouble() / scale);
    }
    return GeoUtils.JTS_FACTORY.createLinearRing(coordinates);
  }

  private static Geometry recordGeometry(JsonNode groups) {
    Polygon[] polygons = new Polygon[groups.size()];
    for (int i = 0; i < groups.size(); i++) {
      var group = groups.get(i);
      var holes = group.get("holes");
      LinearRing[] interior = new LinearRing[holes.size()];
      for (int j = 0; j < holes.size(); j++) {
        interior[j] = ring(holes.get(j), 16);
      }
      polygons[i] = GeoUtils.JTS_FACTORY.createPolygon(ring(group.get("exterior"), 16), interior);
    }
    return polygons.length == 1 ? polygons[0] : GeoUtils.JTS_FACTORY.createMultiPolygon(polygons);
  }

  private static Geometry expectedGeometry(JsonNode polygons) {
    Polygon[] result = new Polygon[polygons.size()];
    for (int i = 0; i < polygons.size(); i++) {
      var rings = polygons.get(i);
      LinearRing[] holes = new LinearRing[rings.size() - 1];
      for (int j = 1; j < rings.size(); j++) {
        holes[j - 1] = ring(rings.get(j), 1);
      }
      result[i] = GeoUtils.JTS_FACTORY.createPolygon(ring(rings.get(0), 1), holes);
    }
    return GeoUtils.JTS_FACTORY.createMultiPolygon(result);
  }

  public static void main(String[] args) throws Exception {
    var fixture = new ObjectMapper().readTree(Path.of(args[0]).toFile());
    boolean allMatch = true;
    for (var testCase : fixture.get("cases")) {
      List<VectorTile.Feature> features = new ArrayList<>();
      for (var record : testCase.get("records")) {
        features.add(new VectorTile.Feature(record.get("layer").asText(), record.get("id").asLong(),
          VectorTile.encodeGeometry(recordGeometry(record.get("geometry").get("Polygon"))), Map.of()));
      }
      var merge = testCase.get("merge");
      var merged = FeatureMerge.mergeNearbyPolygons(features, merge.get(0).asDouble(),
        merge.get(1).asDouble(), merge.get(2).asDouble(), merge.get(3).asDouble(), Stats.inMemory(), null);
      var actual = AffineTransformation.scaleInstance(16, 16).transform(merged.getFirst().geometry().decode());
      if (actual instanceof Polygon polygon) {
        actual = GeoUtils.JTS_FACTORY.createMultiPolygon(new Polygon[]{polygon});
      }
      boolean matches = merged.size() == 1
        && actual.norm().equalsExact(expectedGeometry(testCase.get("expected")).norm());
      allMatch &= matches;
      System.out.println(testCase.get("name").asText() + " matches=" + matches + " " + actual);
    }
    System.exit(allMatch ? 0 : 1);
  }
}
