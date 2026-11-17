import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.file.Path;
import java.util.*;
import org.locationtech.jts.algorithm.CGAlgorithmsDD;
import org.locationtech.jts.algorithm.RobustLineIntersector;
import org.locationtech.jts.geom.Coordinate;

/** Generates exact IEEE-754 fixtures without decimal parser roundtrip ambiguity. */
public class IntersectionArithmeticOracle {
  static List<String> bits(Coordinate p) { return List.of(Long.toUnsignedString(Double.doubleToRawLongBits(p.x),16),Long.toUnsignedString(Double.doubleToRawLongBits(p.y),16)); }
  public static void main(String[] args) throws Exception {
    var mapper=new ObjectMapper(); var input=mapper.readTree(Path.of(args[0]).toFile());
    var segments=new ArrayList<Coordinate[]>();
    for(var ring:input) for(int i=0;i<ring.size();i++) {
      var a=ring.get(i);var b=ring.get((i+1)%ring.size());
      var p=new Coordinate(a.get(0).asDouble(),a.get(1).asDouble());var q=new Coordinate(b.get(0).asDouble(),b.get(1).asDouble());
      if(!p.equals2D(q))segments.add(new Coordinate[]{p,q});
    }
    var li=new RobustLineIntersector(); var output=new ArrayList<Map<String,Object>>();
    for(int i=0;i<segments.size();i++)for(int j=i+1;j<segments.size();j++) {
      var a=segments.get(i);var b=segments.get(j);li.computeIntersection(a[0],a[1],b[0],b[1]);
      if(li.isProper()) {
        var point=CGAlgorithmsDD.intersection(a[0],a[1],b[0],b[1]);
        if(point!=null)output.add(Map.of("a",List.of(bits(a[0]),bits(a[1])),"b",List.of(bits(b[0]),bits(b[1])),"dd",bits(point),"robust",bits(li.getIntersection(0))));
      }
    }
    mapper.writeValue(Path.of(args[1]).toFile(),Map.of("description","Pinned JTS 1.20 CGAlgorithmsDD proper intersections of actual landuse erosion curves; all coordinates stored as IEEE-754 hex bits.","cases",output));
  }
}
