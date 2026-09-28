// NOTLIN: generated from //?/D:/Code/notlin/tests/tmp_objref4/b/dto_kt_dir/dto.kt — do not edit by hand while the source .kt exists
package fixture.objref5;

import lombok.Data;
import lombok.AllArgsConstructor;
import java.util.*;
import java.util.stream.Stream;

import org.jetbrains.annotations.*;

public final class AgentException implements Contract {
    public static final AgentException INSTANCE = new AgentException();
    private AgentException() {}

    private static Object readResolve() {
        return ApplicationInit.INSTANCE;
    }

    public String getType() {
        return "EX";
    }

}
