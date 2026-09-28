public final class InventoryEntry {
    private final int quantity;

    public InventoryEntry(int quantity) {
        this.quantity = quantity;
    }

    public InventoryEntry(InventoryEntry original, int quantity) {
        this.quantity = quantity;
    }

    public int getQuantity() {
        return quantity;
    }
}
