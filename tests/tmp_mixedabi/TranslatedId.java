// NOTLIN: generated from definitions.kt — do not edit by hand while the source .kt exists
package neutral.mixedabi;

import com.fasterxml.jackson.annotation.JsonSubTypes;
import com.fasterxml.jackson.annotation.JsonTypeInfo;
import com.fasterxml.jackson.annotation.JsonIgnore;
import java.util.*;
import java.util.stream.Stream;

import org.jetbrains.annotations.*;

public final class TranslatedId  implements ParentId {

    private final String objectId;
    private final String lookupId;

    public TranslatedId(String objectId, String lookupId) {
        this.objectId = objectId;
        this.lookupId = lookupId;
    }

    public String getObjectId() {
        return objectId;
    }
    public String getLookupId() {
        return lookupId;
    }


    public String toString() {
        return this.getLookupId().toString();
    }
}
