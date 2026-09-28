// NOTLIN: generated from //?/D:/Code/notlin/tests/tmp_dsp/model.kt — do not edit by hand while the source .kt exists
package fixture.dsp;

import java.io.Serializable;
import java.util.ArrayList;
import lombok.Data;
import lombok.AllArgsConstructor;
import java.util.*;
import java.util.stream.Stream;

import org.jetbrains.annotations.*;

@Data

public class DisplayGridRef {
    private final String name;
    private final ArrayList<DisplayRow> rows;

    public DisplayGridRef(String name, ArrayList<DisplayRow> rows) {
        this.name = name;
        this.rows = rows;
    }
    @NotNull public String getName() { return name; }
    @NotNull public ArrayList<DisplayRow> getRows() { return rows; }

    public DisplayGridRef(String name) {
        this(name, new ArrayList<>());
    }

    public DisplayRow add(String key, Object... rowData) {
        DisplayRow row = new DisplayRow(key, java.util.stream.Stream.of(rowData).toArray(), null);
        this.getRows().add(row);
        return row;
    }

}
