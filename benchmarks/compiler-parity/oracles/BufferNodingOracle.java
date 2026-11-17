import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.algorithm.RobustLineIntersector;
import org.locationtech.jts.geom.*;
import org.locationtech.jts.geomgraph.Label;
import org.locationtech.jts.noding.*;
import org.locationtech.jts.operation.buffer.*;

/** Dumps pinned floating buffer curves and noded edges, with original side labels.
 * Input is an array of {exterior:[{x,y}],holes:[[{x,y}]]} in MVT units.
 */
public class BufferNodingOracle {
  record Trace(int sourceCurve, int left, int right) {}
  static final GeometryFactory FACTORY = new GeometryFactory();
  static LinearRing ring(JsonNode input) {
    Coordinate[] coordinates = new Coordinate[input.size()];
    for (int i=0;i<coordinates.length;i++) coordinates[i] = new Coordinate(input.get(i).get("x").asDouble(),input.get(i).get("y").asDouble());
    return FACTORY.createLinearRing(coordinates);
  }
  static List<List<Double>> points(Coordinate[] coordinates) {
    var result = new ArrayList<List<Double>>();
    for (var p:coordinates) result.add(List.of(p.x,p.y));
    return result;
  }
  static Map<String,Object> row(SegmentString segment) {
    Trace data=(Trace)segment.getData();
    return Map.of("source_curve",data.sourceCurve(),"left_location",data.left(),"right_location",data.right(),"coordinates",points(segment.getCoordinates()));
  }
  public static void main(String[] args) throws Exception {
    var mapper = new ObjectMapper();
    var input = mapper.readTree(Path.of(args[0]).toFile());
    var polygons = new ArrayList<Polygon>();
    for (var p:input) {
      var holes = new ArrayList<LinearRing>();
      for (var h:p.get("holes")) holes.add(ring(h));
      polygons.add(FACTORY.createPolygon(ring(p.get("exterior")),holes.toArray(LinearRing[]::new)));
    }
    var geometry = FACTORY.createMultiPolygon(polygons.toArray(Polygon[]::new));
    var parameters = new BufferParameters(); parameters.setJoinStyle(BufferParameters.JOIN_MITRE); parameters.setMitreLimit(5);
    var builder = new BufferCurveSetBuilder(geometry,Double.parseDouble(args[1]),new PrecisionModel(),parameters);
    var curves = new ArrayList<NodedSegmentString>();
    var raw = new ArrayList<Map<String,Object>>();
    for (Object object:builder.getCurves()) {
      var segment=(SegmentString)object; var label=(Label)segment.getData();
      var trace=new Trace(curves.size(),label.getLocation(0,Position.LEFT),label.getLocation(0,Position.RIGHT));
      var curve=new NodedSegmentString(segment.getCoordinates(),trace); curves.add(curve); raw.add(row(curve));
    }
    var noder = new MCIndexNoder(new IntersectionAdder(new RobustLineIntersector()));
    noder.computeNodes(curves);
    var noded = new ArrayList<Map<String,Object>>();
    for(Object object:noder.getNodedSubstrings()) noded.add(row((SegmentString)object));
    mapper.writeValue(Path.of(args[2]).toFile(),Map.of("distance_mvt",Double.parseDouble(args[1]),"raw_curves",raw,"noded_edges",noded));
  }
}
