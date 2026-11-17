import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.SerializationFeature;
import com.onthegomap.planetiler.VectorTile;
import com.onthegomap.planetiler.geo.GeoUtils;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.algorithm.RobustLineIntersector;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.LinearRing;
import org.locationtech.jts.geom.Polygon;
import org.locationtech.jts.geom.PrecisionModel;
import org.locationtech.jts.geomgraph.Label;
import org.locationtech.jts.noding.IntersectionAdder;
import org.locationtech.jts.noding.MCIndexNoder;
import org.locationtech.jts.noding.NodedSegmentString;
import org.locationtech.jts.noding.SegmentString;
import org.locationtech.jts.operation.buffer.BufferCurveSetBuilder;
import org.locationtech.jts.operation.buffer.BufferOp;
import org.locationtech.jts.operation.buffer.BufferParameters;

/** Bit-exact stage trace for the captured landuse z7/70/43 residential cluster. */
public final class LanduseBitsOracle {
  private static final double MVT_SCALE = 16.0;

  private static String bits(double tilePixels) {
    return String.format("%016x", Double.doubleToRawLongBits(tilePixels * MVT_SCALE));
  }

  private static List<List<String>> points(Coordinate[] coordinates) {
    var result = new ArrayList<List<String>>(coordinates.length);
    for (var point : coordinates) {
      result.add(List.of(bits(point.x), bits(point.y)));
    }
    return result;
  }

  private static LinearRing ring(JsonNode input) {
    Coordinate[] coordinates = new Coordinate[input.size()];
    for (int i = 0; i < input.size(); i++) {
      JsonNode point = input.get(i);
      if (!point.get("x").isIntegralNumber() || !point.get("y").isIntegralNumber()) {
        throw new IllegalArgumentException("source coordinates must be original MVT integers");
      }
      coordinates[i] = new Coordinate(point.get("x").longValue() / MVT_SCALE,
          point.get("y").longValue() / MVT_SCALE);
    }
    return GeoUtils.JTS_FACTORY.createLinearRing(coordinates);
  }

  private static Geometry geometry(JsonNode groups) {
    var polygons = new Polygon[groups.size()];
    for (int i = 0; i < groups.size(); i++) {
      JsonNode group = groups.get(i);
      JsonNode holes = group.get("holes");
      var interior = new LinearRing[holes.size()];
      for (int j = 0; j < holes.size(); j++) {
        interior[j] = ring(holes.get(j));
      }
      polygons[i] = GeoUtils.JTS_FACTORY.createPolygon(ring(group.get("exterior")), interior);
    }
    return polygons.length == 1 ? polygons[0] : GeoUtils.JTS_FACTORY.createMultiPolygon(polygons);
  }

  private static Map<String, Object> polygonBits(Geometry geometry) {
    var polygons = new ArrayList<Map<String, Object>>();
    for (int i = 0; i < geometry.getNumGeometries(); i++) {
      Geometry component = geometry.getGeometryN(i);
      if (!(component instanceof Polygon polygon)) {
        throw new IllegalArgumentException("expected polygonal stage, got " + component.getGeometryType());
      }
      var holes = new ArrayList<List<List<String>>>();
      for (int j = 0; j < polygon.getNumInteriorRing(); j++) {
        holes.add(points(polygon.getInteriorRingN(j).getCoordinates()));
      }
      polygons.add(Map.of("exterior", points(polygon.getExteriorRing().getCoordinates()),
          "holes", holes));
    }
    return Map.of("geometry_type", geometry.getGeometryType(), "polygons", polygons);
  }

  private static Map<String, Object> curveBits(SegmentString curve, int index,
      int left, int right) {
    return Map.of("source_curve", index, "left_location", left,
        "right_location", right, "coordinates", points(curve.getCoordinates()));
  }

  public static void main(String[] args) throws Exception {
    if (args.length != 4) {
      throw new IllegalArgumentException("usage: LanduseBitsOracle PREMERGE CLUSTERS INDEX OUTPUT");
    }
    var mapper = new ObjectMapper();
    mapper.configure(SerializationFeature.ORDER_MAP_ENTRIES_BY_KEYS, true);
    JsonNode records = mapper.readTree(Path.of(args[0]).toFile());
    JsonNode members = mapper.readTree(Path.of(args[1]).toFile()).get(Integer.parseInt(args[2])).get("members");
    if (members.size() != 87) {
      throw new IllegalArgumentException("expected captured 87-member cluster; got " + members.size());
    }
    var originals = new ArrayList<Geometry>();
    var sourceIds = new ArrayList<Long>();
    var sourceIndices = new ArrayList<Integer>();
    for (JsonNode member : members) {
      int index = member.intValue();
      JsonNode record = records.get(index);
      sourceIndices.add(index);
      sourceIds.add(record.get("id").longValue());
      Geometry source = geometry(record.get("geometry").get("Polygon"));
      originals.add(VectorTile.encodeGeometry(source).decode());
    }

    double distance = 0.25; // z7 residential landuse, tile pixels = 4 MVT units.
    var parameters = new BufferParameters();
    parameters.setJoinStyle(BufferParameters.JOIN_MITRE);
    var buffered = new ArrayList<Geometry>();
    for (Geometry original : originals) {
      buffered.add(new BufferOp(original, parameters).getResultGeometry(distance));
    }
    Geometry unioned = GeoUtils.createGeometryCollection(buffered).union();
    Geometry contracted = new BufferOp(unioned, parameters).getResultGeometry(-distance);
    Geometry snapped = GeoUtils.snapAndFixPolygon(contracted, null, "landuse-bits-oracle").reverse();

    var rawCurves = new ArrayList<Map<String, Object>>();
    var nodedCurves = new ArrayList<Map<String, Object>>();
    var curves = new ArrayList<NodedSegmentString>();
    var builder = new BufferCurveSetBuilder(unioned, -distance, new PrecisionModel(), parameters);
    for (Object object : builder.getCurves()) {
      var segment = (SegmentString) object;
      var label = (Label) segment.getData();
      int sourceCurve = curves.size();
      int left = label.getLocation(0, org.locationtech.jts.geom.Position.LEFT);
      int right = label.getLocation(0, org.locationtech.jts.geom.Position.RIGHT);
      var curve = new NodedSegmentString(segment.getCoordinates(),
          new int[] {sourceCurve, left, right});
      curves.add(curve);
      rawCurves.add(curveBits(curve, sourceCurve, left, right));
    }
    var noder = new MCIndexNoder(new IntersectionAdder(new RobustLineIntersector()));
    noder.computeNodes(curves);
    for (Object object : noder.getNodedSubstrings()) {
      var segment = (SegmentString) object;
      int[] label = (int[]) segment.getData();
      nodedCurves.add(curveBits(segment, label[0], label[1], label[2]));
    }

    var output = new LinkedHashMap<String, Object>();
    output.put("coordinate_encoding", "IEEE-754 binary64 raw bits, 16-digit lowercase hex, MVT units");
    output.put("tile", List.of(7, 70, 43));
    output.put("cluster_index", Integer.parseInt(args[2]));
    output.put("distance_mvt", 4);
    output.put("source_indices", sourceIndices);
    output.put("source_ids", sourceIds);
    output.put("buffered", buffered.stream().map(LanduseBitsOracle::polygonBits).toList());
    output.put("unioned", polygonBits(unioned));
    output.put("contracted", polygonBits(contracted));
    output.put("snapped", polygonBits(snapped));
    output.put("negative_buffer_raw_curves", rawCurves);
    output.put("negative_buffer_noded_edges", nodedCurves);
    mapper.writeValue(Path.of(args[3]).toFile(), output);
  }
}
