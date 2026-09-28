// NOTLIN: generated from definitions.kt — do not edit by hand while the source .kt exists
package neutral.mixedabi;

import com.fasterxml.jackson.annotation.JsonSubTypes;
import com.fasterxml.jackson.annotation.JsonTypeInfo;
import com.fasterxml.jackson.annotation.JsonIgnore;
import java.util.*;
import java.util.stream.Stream;

import org.jetbrains.annotations.*;

@Retention(AnnotationRetention.RUNTIME)
@JsonTypeInfo(
    use = JsonTypeInfo.Id.NAME,
    include = JsonTypeInfo.As.PROPERTY,
    property = "_class"
)
@JsonSubTypes({@JsonSubTypes.Type(value = RetainedId.class),
    @JsonSubTypes.Type(value = TranslatedId.class)})
public interface ParentId {
    String getObjectId();

    String getLookupId();

}
