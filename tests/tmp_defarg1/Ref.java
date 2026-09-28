// NOTLIN: generated from //?/D:/Code/notlin/tests/tmp_defarg1/model.kt — do not edit by hand while the source .kt exists
package fixture.defarg1;

import lombok.Data;
import lombok.AllArgsConstructor;
import java.util.*;
import java.util.stream.Stream;

import org.jetbrains.annotations.*;

@Data
@AllArgsConstructor

public class Ref {
    private final String name;

    public Row add(String key, Object... row) {
        Row r = new Row(key, java.util.stream.Stream.of(row).toArray(), null);
        return r;
    }

}
