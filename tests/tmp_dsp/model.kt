package fixture.dsp
import java.io.Serializable
import java.util.ArrayList
        // NOTLIN: NC5BA top-level construct not supported: assignment
        // NOTLIN: N3F4B top-level construct not supported: ERROR
        defaultImpl = DisplayData::class)
// NOTLIN: N7395 workspace Kotlin implementation requires this declaration to remain Kotlin
@JsonSubTypes(
        JsonSubTypes.Type(value = DisplayData::class),
        JsonSubTypes.Type(value = DisplayWidgets::class)
)
interface IDisplayData : Serializable {
    var title: DisplayMessage
}

// NOTLIN: N7395 workspace Kotlin implementation requires this declaration to remain Kotlin
@JsonTypeName("data")
@Deprecated("Use new DisplayGrid / DisplayFields")
data class DisplayData(
        override var title: DisplayMessage,
        var columns: List<String> = arrayListOf(),
        var rows: MutableList<Array<String>> = arrayListOf()
) : IDisplayData {
    constructor(title: IDisplayMessageType, vararg columns: String) : this(DisplayMessage(title), columns.asList(), mutableListOf())

    constructor(title: DisplayMessage, vararg columns: String) : this(title, columns.asList(), mutableListOf())

    fun add(vararg rowData: String) {
        rows.add(arrayOf(*rowData));
    }
}

// NOTLIN: N7395 workspace Kotlin implementation requires this declaration to remain Kotlin
@JsonTypeInfo(
        use = JsonTypeInfo.Id.NAME,
        include = JsonTypeInfo.As.EXISTING_PROPERTY,
        property = "type")
// NOTLIN: N04DC declaration annotation is retained in Kotlin
@JsonSubTypes(
        JsonSubTypes.Type(value = DisplayGrid::class),
        JsonSubTypes.Type(value = DisplayFields::class),
        JsonSubTypes.Type(value = DisplayAction::class),
        JsonSubTypes.Type(value = DisplayRawData::class)
)
interface IDisplayWidget : Serializable {
    val name: String
    val type : DisplayWidgetType
}

// NOTLIN: N7395 workspace Kotlin implementation requires this declaration to remain Kotlin
data class DisplayWidgets(
        override var title: DisplayMessage,
        var widgets: List<IDisplayWidget>
) : IDisplayData {
    constructor(title: IDisplayMessage) : this(title.asDisplayMessage(), arrayListOf())
    constructor(title: IDisplayMessage, widget: IDisplayWidget) : this(title.asDisplayMessage(), arrayListOf(widget))
    constructor(title: IDisplayMessage, widgets: List<IDisplayWidget>) : this(title.asDisplayMessage(), widgets)

    fun add(widget: IDisplayWidget) : DisplayWidgets {
        return DisplayWidgets(title, this.widgets+widget)
    }

    fun add(widgets: List<IDisplayWidget>) : DisplayWidgets {
        return DisplayWidgets(title, this.widgets+widgets)
    }
}

// NOTLIN: N7395 workspace Kotlin implementation requires this declaration to remain Kotlin
data class DisplayField @JvmOverloads constructor(
        val name: String,
        val label: String,
        val type: DisplayDataType,
        val action: DisplayFieldAction = DisplayFieldAction.NONE,
        val value: Any? = null
) {
    companion object {
        @JvmStatic
        fun string(name : String, label: String) : DisplayField {
            return DisplayField(name, label, DisplayDataType.STRING)
        }

        @JvmStatic
        fun string(name : String) : DisplayField {
            return string(name, name)
        }

        @JvmStatic
        fun number(name : String, label: String) : DisplayField {
            return DisplayField(name, label, DisplayDataType.NUMBER)
        }

        @JvmStatic
        fun number(name : String) : DisplayField {
            return number(name, name)
        }

        @JvmStatic
        fun quantity(name : String, label: String) : DisplayField {
            return DisplayField(name, label, DisplayDataType.QUANTITY)
        }

        @JvmStatic
        fun quantity(name : String) : DisplayField {
            return quantity(name, name)
        }

        @JvmStatic
        fun time(name : String, label: String) : DisplayField {
            return DisplayField(name, label, DisplayDataType.TIME)
        }

        @JvmStatic
        fun time(name : String) : DisplayField {
            return time(name, name)
        }

        @JvmStatic
        fun barcode(name : String, label: String) : DisplayField {
            return DisplayField(name, label, DisplayDataType.BARCODE)
        }

        @JvmStatic
        fun barcode(name : String) : DisplayField {
            return barcode(name, name)
        }
    }
}

// NOTLIN: N7395 workspace Kotlin implementation requires this declaration to remain Kotlin
@JsonTypeName("FIELDS")
data class DisplayFields @JvmOverloads constructor(
        override val name: String,
        val elements: MutableList<DisplayField> = arrayListOf()
) : IDisplayWidget {

    fun add(name: String, label: String, type: DisplayDataType, value: Any) {
        elements.add(DisplayField(name, label, type, DisplayFieldAction.NONE, value))
    }

    fun add(name: String, type: DisplayDataType, value: Any) {
        return add(name, name, type, value)
    }

    override val type: DisplayWidgetType
        get() = DisplayWidgetType.FIELDS
}

// NOTLIN: N7395 workspace Kotlin implementation requires this declaration to remain Kotlin
@JsonTypeName("GRID")
data class DisplayGrid @JvmOverloads constructor(
        override val name: String,
        var rowAction: DisplayRowAction = DisplayRowAction.NONE,
        var columns: MutableList<DisplayField> = arrayListOf(),
        var rows: MutableList<DisplayRow> = arrayListOf(),
        var grids: MutableList<DisplayGrid> = arrayListOf()
) : IDisplayWidget {

    fun addColumn(name: String, label: String, type: DisplayDataType) {
        columns.add(DisplayField(name, label, type))
    }

    fun add(key: String, vararg rowData: Any) : DisplayRow {
        val row = DisplayRow(key, arrayOf(*rowData))
        rows.add(row)
        return row
    }

    fun add(grid: DisplayGrid) {
        grids.add(grid)
    }

    override val type: DisplayWidgetType
        get() = DisplayWidgetType.GRID
}

// NOTLIN: N7395 workspace Kotlin implementation requires this declaration to remain Kotlin
data class DisplayRow(
        val key: String,
        val data: Array<Any>,
        var grid: DisplayGrid? = null
)

@JsonTypeName("ACTION")
