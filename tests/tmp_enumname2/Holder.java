// NOTLIN: generated from D:/Code/notlin/tests/tmp_enumname2/m.kt — do not edit by hand while the source .kt exists
package e;

import java.util.*;
import java.util.stream.Stream;

import org.jetbrains.annotations.*;

public class Holder {
    private final Tag type;

    public Holder(Tag type) {
        this.type = type;
    }

    public Tag getType() {
        return type;
    }

    public String toString() {
        return this.getType().name();
    }

}
