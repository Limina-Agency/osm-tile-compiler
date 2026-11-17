import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.onthegomap.planetiler.FeatureMerge;
import com.onthegomap.planetiler.VectorTile;
import com.onthegomap.planetiler.geo.GeoUtils;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.LinearRing;
import org.locationtech.jts.geom.Polygon;
import org.locationtech.jts.operation.buffer.BufferOp;
import org.locationtech.jts.operation.buffer.BufferParameters;

/** Replays native rendered inputs through the pinned postmerge implementation. */
public class LanduseMergeOracle {
  private static LinearRing ring(JsonNode points) {
    Coordinate[] coordinates = new Coordinate[points.size()];
    for (int i = 0; i < points.size(); i++) {
      var point = points.get(i);
      coordinates[i] = new Coordinate(point.get("x").asDouble() / 16,
          point.get("y").asDouble() / 16);
    }
    return GeoUtils.JTS_FACTORY.createLinearRing(coordinates);
  }

  private static Geometry geometry(JsonNode groups) {
    Polygon[] polygons = new Polygon[groups.size()];
    for (int i = 0; i < groups.size(); i++) {
      var group = groups.get(i);
      var holes = group.get("holes");
      LinearRing[] interior = new LinearRing[holes.size()];
      for (int j = 0; j < holes.size(); j++) {
        interior[j] = ring(holes.get(j));
      }
      polygons[i] = GeoUtils.JTS_FACTORY.createPolygon(ring(group.get("exterior")), interior);
    }
    return polygons.length == 1 ? polygons[0] : GeoUtils.JTS_FACTORY.createMultiPolygon(polygons);
  }

  public static void main(String[] args) throws Exception {
    var input = new ObjectMapper().readTree(Path.of(args[0]).toFile());
    List<VectorTile.Feature> features = new ArrayList<>();
    for (var record : input) {
      features.add(new VectorTile.Feature("landuse", record.get("id").asLong(),
          VectorTile.encodeGeometry(geometry(record.get("geometry").get("Polygon"))),
          Map.of("class", "residential")));
    }
    List<Geometry> decoded = new ArrayList<>();
    for (var feature : features) {
      decoded.add(feature.geometry().decode());
    }
    var clusters = FeatureMerge.groupPolygonsByProximity(decoded, 0.1);
    var sizes = clusters.stream().map(List::size).sorted().toList();
    System.err.println("clusters=" + clusters.size() + " singleton_clusters="
        + sizes.stream().filter(size -> size == 1).count() + " sizes=" + sizes);
    if (args.length > 1) {
      var indices = new IdentityHashMap<Geometry, Integer>();
      for (int i = 0; i < decoded.size(); i++) {
        indices.put(decoded.get(i), i);
      }
      var details = new ArrayList<Map<String, Object>>();
      for (var cluster : clusters) {
        var members = cluster.stream().map(indices::get).sorted().toList();
        var subset = members.stream().map(features::get).toList();
        var output = FeatureMerge.mergeNearbyPolygons(subset, 1, 1, 0.1, 0.1);
        var shapes = new ArrayList<String>();
        for (var feature : output) {
          shapes.add(feature.geometry().decode().toText());
        }
        details.add(Map.of("members", members, "wkt", shapes));
      }
      new ObjectMapper().writeValue(Path.of(args[1]).toFile(), details);
    }
    if (args.length > 2) {
      var parameters = new BufferParameters();
      parameters.setJoinStyle(BufferParameters.JOIN_MITRE);
      var originals = List.of(decoded.get(0), decoded.get(251));
      var buffered = originals.stream()
          .map(shape -> new BufferOp(shape, parameters).getResultGeometry(0.1)).toList();
      var combined = GeoUtils.createGeometryCollection(buffered);
      var unioned = combined.union();
      var contracted = new BufferOp(unioned, parameters).getResultGeometry(-0.1);
      var snapped = GeoUtils.snapAndFixPolygon(contracted, null, "landuse-stage-oracle").reverse();
      var stages = Map.of(
          "input", originals.stream().map(Geometry::toText).toList(),
          "buffered", buffered.stream().map(Geometry::toText).toList(),
          "unioned", unioned.toText(),
          "contracted", contracted.toText(),
          "snapped", snapped.toText());
      new ObjectMapper().writeValue(Path.of(args[2]).toFile(), stages);
    }
    var merged = FeatureMerge.mergeNearbyPolygons(features, 1, 1, 0.1, 0.1);
    System.err.println("inputs=" + features.size() + " outputs=" + merged.size());
    for (var feature : merged) {
      Geometry shape = feature.geometry().decode();
      System.err.println("id=" + feature.id() + " area_px2=" + shape.getArea()
          + " components=" + shape.getNumGeometries() + " points=" + shape.getNumPoints());
      System.out.println(shape.toText());
    }
  }
}
