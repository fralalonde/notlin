package com.onomatic.tes.coreapi.device.application

import com.fasterxml.jackson.annotation.JsonIgnore
import com.fasterxml.jackson.annotation.JsonSubTypes
import com.fasterxml.jackson.annotation.JsonTypeInfo
import com.fasterxml.jackson.annotation.JsonTypeName
import com.onomatic.tes.coreapi.agent.message.IAgentMessageData
import com.onomatic.tes.coreapi.application.IApplicationAction
import com.onomatic.tes.coreapi.application.message.ApplicationEventType
import com.onomatic.tes.coreapi.application.message.IApplicationInputType
import com.onomatic.tes.coreapi.application.message.IApplicationMessageType
import com.onomatic.tes.coreapi.device.DeviceMessageType
import java.io.Serializable
import java.util.*

@JsonTypeInfo(
        use = JsonTypeInfo.Id.NAME,
        include = JsonTypeInfo.As.PROPERTY,
        property = "_class",
        defaultImpl = DisplayData::class)
@JsonSubTypes(
        JsonSubTypes.Type(value = DisplayData::class),
        JsonSubTypes.Type(value = DisplayWidgets::class)
)
interface IDisplayData : Serializable {
    var title: DisplayMessage
}


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

@JsonTypeInfo(
        use = JsonTypeInfo.Id.NAME,
        include = JsonTypeInfo.As.EXISTING_PROPERTY,
        property = "type")
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

enum class DisplayWidgetType {
    GRID,
    FIELDS,
    ACTION,
    RAW_DATA
}

enum class DisplayDataType {
    STRING,
    MULTI_LINE_STRING,
    NUMBER,
    DATE,
    TIME,
    DATE_TIME,
    QUANTITY,
    BARCODE,
    IMAGE
}

enum class DisplayFieldAction {
    NONE,
    SCAN;
}

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

enum class DisplayRowAction {
    NONE,
    KEY_SCAN,
    KEY_DRILLDOWN;
}

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

data class DisplayRow(
        val key: String,
        val data: Array<Any>,
        var grid: DisplayGrid? = null
)

data class DisplayGridRef @JvmOverloads constructor(
        val name: String,
        val rows: MutableList<DisplayRow> = arrayListOf()
) {
    fun add(key: String, vararg rowData: Any) : DisplayRow {
        val row = DisplayRow(key, arrayOf(*rowData))
        rows.add(row)
        return row
    }
}

@JsonTypeName("ACTION")
data class DisplayAction(
        var label: String,
        var color: String,
        var action: IApplicationAction
) : IDisplayWidget {

    companion object {
        //// Shorthand for https://quasar.dev/style/color-palette#brand-colors
        const val COLOR_PRIMARY: String = "primary"
        const val COLOR_SECONDARY: String = "secondary"
        const val COLOR_ACCENT: String = "accent"
        const val COLOR_DARK: String = "dark"
        const val COLOR_POSITIVE: String = "positive"
        const val COLOR_NEGATIVE: String = "negative"
        const val COLOR_INFO: String = "info"
        const val COLOR_WARNING: String = "warning"
    }

    constructor(action: IApplicationAction) : this(action.alias.uppercase(Locale.getDefault()), COLOR_NEGATIVE, action)
    constructor(action: IApplicationAction, color: String) : this(action.alias.uppercase(Locale.getDefault()), color, action)

    override val type: DisplayWidgetType
        get() = DisplayWidgetType.ACTION

    override val name: String
        get() = label
}

@JsonTypeName("RAW_DATA")
data class DisplayRawData(
        override val name: String,
        var data: String?
) : IDisplayWidget {
    override val type: DisplayWidgetType
        get() = DisplayWidgetType.RAW_DATA
}

data class DisplayMessage @JvmOverloads constructor(
        val type: IDisplayMessageType,
        val values: Map<String, Any> = hashMapOf()
) : IDisplayMessage, Serializable {
    val messageType : DeviceMessageType
        get() = type.type

    override fun asDisplayMessage(): DisplayMessage {
        return this
    }
}

// This type shouldn't be used often since the free-form message won't be localized
enum class DefaultMessage(
        override val alias: String,
        override val type: DeviceMessageType
) : IDisplayMessageType {
    REQUEST("request", DeviceMessageType.REQUEST),
    CONFIRMATION("confirmation", DeviceMessageType.CONFIRMATION),
    INFORMATION("information", DeviceMessageType.INFORMATION),
    ERROR("error", DeviceMessageType.ERROR),
    WARNING("warning", DeviceMessageType.WARNING);

    override val template: String
        get() = "{message}"
    override val baseAlias: String
        get() = "string/message"
}

@JsonTypeInfo(
        use = JsonTypeInfo.Id.NAME,
        include = JsonTypeInfo.As.EXISTING_PROPERTY,
        property = "type")
@JsonSubTypes(
        JsonSubTypes.Type(value = ApplicationInit::class),
        JsonSubTypes.Type(value = ApplicationLogout::class),
        JsonSubTypes.Type(value = ApplicationInput::class)
)
interface IApplicationMessageData : IAgentMessageData {
    override val type: IApplicationMessageType
}

@JsonTypeInfo(
        use = JsonTypeInfo.Id.NAME,
        include = JsonTypeInfo.As.EXISTING_PROPERTY,
        property = "type")
@JsonSubTypes(
        JsonSubTypes.Type(value = ApplicationActionExecuted::class),
        JsonSubTypes.Type(value = ApplicationBarcodeScanned::class)
)
interface ApplicationInput : IApplicationMessageData  {
    override val type: IApplicationInputType
}

enum class ApplicationInputType(
        override val alias: String
) : IApplicationInputType {
    BARCODE_SCANNED("barcode"),
    ACTION_EXECUTED("action"),
    ;
    override val baseAlias: String
        get() = "application/input"
}

@JsonTypeName(value = "application/init")
object ApplicationInit : IApplicationMessageData {
    private fun readResolve(): Any = ApplicationInit

    override val type: ApplicationEventType
        get() = ApplicationEventType.INIT
}

@JsonTypeName(value = "application/logout")
data class ApplicationLogout(
        val timeout: Boolean
) : IApplicationMessageData {
    override val type: ApplicationEventType
        get() = ApplicationEventType.LOGOUT
}

@JsonTypeName(value = "application/input/barcode")
data class ApplicationBarcodeScanned(
        val barcode : String
) : ApplicationInput {
    @get:JsonIgnore
    override val referenceNo: String
        get() = barcode

    override val type: IApplicationInputType
        get() = ApplicationInputType.BARCODE_SCANNED
}

@JsonTypeName(value = "application/input/action")
data class ApplicationActionExecuted @JvmOverloads constructor(
        val action : IApplicationAction,
        val parameters: Map<String, Object>? = null
) : ApplicationInput {
    @get:JsonIgnore
    override val referenceNo: String
        get() = action.getKey()

    override val type: IApplicationInputType
        get() = ApplicationInputType.ACTION_EXECUTED
}
