import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.onthegomap.planetiler.FeatureMerge;
import com.onthegomap.planetiler.VectorTile;
import com.onthegomap.planetiler.geo.GeoUtils;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.LinearRing;
import org.locationtech.jts.geom.Polygon;
import org.locationtech.jts.operation.buffer.BufferOp;
import org.locationtech.jts.operation.buffer.BufferParameters;

/** Replays integer polygon records through pinned zero-distance merging. */
public class PolygonUnionOracle {
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
    var mapper = new ObjectMapper();
    var fixture = mapper.readTree(Path.of(args[0]).toFile());
    var input = fixture.has("input") ? fixture.get("input") : fixture;
    List<VectorTile.Feature> features = new ArrayList<>();
    for (var record : input) features.add(new VectorTile.Feature("landcover", record.get("id").asLong(),
      VectorTile.encodeGeometry(geometry(record.get("geometry").get("Polygon"))), Map.of()));
    var output = new ArrayList<Map<String,Object>>();
    for (var feature: FeatureMerge.mergeOverlappingPolygons(features,4)) {
      var geometry=feature.geometry().decode();
      output.add(Map.of("id",feature.id(),"wkt_pixels",geometry.toText(),"area_pixels2",geometry.getArea()));
    }
    mapper.writeValue(Path.of(args[1]).toFile(),output);
  }
}
