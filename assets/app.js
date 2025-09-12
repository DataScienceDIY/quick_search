// QuickSearch Application JavaScript

// Enhanced UI interactions
document.addEventListener('DOMContentLoaded', function() {
    console.log('QuickSearch UI loaded');
    
    // Add loading states to buttons
    function addLoadingState(button, originalText) {
        button.disabled = true;
        button.innerHTML = '<span class="loading"></span>' + originalText;
    }
    
    function removeLoadingState(button, originalText) {
        button.disabled = false;
        button.innerHTML = originalText;
    }
    
    // Enhanced form interactions
    const forms = document.querySelectorAll('form');
    forms.forEach(form => {
        form.addEventListener('submit', function(e) {
            const submitButton = form.querySelector('button[type="submit"]');
            if (submitButton) {
                addLoadingState(submitButton, submitButton.textContent);
            }
        });
    });
    
    // Keyboard shortcuts
    document.addEventListener('keydown', function(e) {
        // Ctrl+F to focus search
        if (e.ctrlKey && e.key === 'f') {
            e.preventDefault();
            const searchInput = document.querySelector('input[type="text"]');
            if (searchInput) {
                searchInput.focus();
                searchInput.select();
            }
        }
        
        // Escape to clear search
        if (e.key === 'Escape') {
            const searchInput = document.querySelector('input[type="text"]');
            if (searchInput && searchInput === document.activeElement) {
                searchInput.value = '';
                searchInput.blur();
            }
        }
    });
    
    // Enhanced table interactions
    function enhanceTable(table) {
        // Add click-to-copy functionality for table cells
        const cells = table.querySelectorAll('td');
        cells.forEach(cell => {
            cell.addEventListener('click', function() {
                const text = cell.textContent.trim();
                if (text && navigator.clipboard) {
                    navigator.clipboard.writeText(text).then(() => {
                        // Visual feedback
                        cell.style.backgroundColor = '#4CAF50';
                        cell.style.color = 'white';
                        setTimeout(() => {
                            cell.style.backgroundColor = '';
                            cell.style.color = '';
                        }, 200);
                    });
                }
            });
        });
        
        // Add sortable columns (basic implementation)
        const headers = table.querySelectorAll('th');
        headers.forEach((header, index) => {
            header.style.cursor = 'pointer';
            header.addEventListener('click', () => sortTable(table, index));
        });
    }
    
    // Simple table sorting
    function sortTable(table, columnIndex) {
        const tbody = table.querySelector('tbody');
        const rows = Array.from(tbody.querySelectorAll('tr'));
        
        rows.sort((a, b) => {
            const aVal = a.cells[columnIndex]?.textContent.trim() || '';
            const bVal = b.cells[columnIndex]?.textContent.trim() || '';
            
            // Try numeric sort first
            const aNum = parseFloat(aVal);
            const bNum = parseFloat(bVal);
            
            if (!isNaN(aNum) && !isNaN(bNum)) {
                return aNum - bNum;
            }
            
            // Fall back to string sort
            return aVal.localeCompare(bVal);
        });
        
        // Clear tbody and re-append sorted rows
        tbody.innerHTML = '';
        rows.forEach(row => tbody.appendChild(row));
    }
    
    // Auto-enhance any tables that appear
    const observer = new MutationObserver(function(mutations) {
        mutations.forEach(function(mutation) {
            mutation.addedNodes.forEach(function(node) {
                if (node.nodeType === 1) { // Element node
                    const tables = node.querySelectorAll ? node.querySelectorAll('table') : [];
                    tables.forEach(enhanceTable);
                    
                    if (node.tagName === 'TABLE') {
                        enhanceTable(node);
                    }
                }
            });
        });
    });
    
    observer.observe(document.body, { childList: true, subtree: true });
    
    // Enhance existing tables
    document.querySelectorAll('table').forEach(enhanceTable);
});

// Utility functions for Rust integration
window.QuickSearch = {
    // Function to show toast notifications
    showToast: function(message, type = 'info') {
        const toast = document.createElement('div');
        toast.className = `toast toast-${type}`;
        toast.textContent = message;
        toast.style.cssText = `
            position: fixed;
            top: 20px;
            right: 20px;
            padding: 12px 20px;
            border-radius: 6px;
            color: white;
            font-weight: 600;
            z-index: 2000;
            animation: slideIn 0.3s ease;
        `;
        
        // Set background based on type
        const colors = {
            info: '#2196F3',
            success: '#4CAF50',
            warning: '#FF9800',
            error: '#f44336'
        };
        toast.style.backgroundColor = colors[type] || colors.info;
        
        document.body.appendChild(toast);
        
        setTimeout(() => {
            toast.style.animation = 'slideOut 0.3s ease';
            setTimeout(() => {
                document.body.removeChild(toast);
            }, 300);
        }, 3000);
    },
    
    // Function to update status display
    updateStatus: function(status) {
        const statusDisplay = document.querySelector('.status-display');
        if (statusDisplay) {
            statusDisplay.textContent = status;
        }
    },
    
    // Function to highlight search terms in results
    highlightSearchTerms: function(searchTerm, container) {
        if (!searchTerm || !container) return;
        
        const walker = document.createTreeWalker(
            container,
            NodeFilter.SHOW_TEXT,
            null,
            false
        );
        
        const textNodes = [];
        let node;
        while (node = walker.nextNode()) {
            textNodes.push(node);
        }
        
        textNodes.forEach(textNode => {
            const parent = textNode.parentNode;
            if (parent.tagName === 'B') return; // Skip already highlighted
            
            const text = textNode.textContent;
            const regex = new RegExp(`(${searchTerm})`, 'gi');
            
            if (regex.test(text)) {
                const highlightedHTML = text.replace(regex, '<mark>$1</mark>');
                const wrapper = document.createElement('span');
                wrapper.innerHTML = highlightedHTML;
                parent.replaceChild(wrapper, textNode);
            }
        });
    }
};

// Add custom CSS for toasts and animations
const style = document.createElement('style');
style.textContent = `
    @keyframes slideIn {
        from { transform: translateX(100%); opacity: 0; }
        to { transform: translateX(0); opacity: 1; }
    }
    
    @keyframes slideOut {
        from { transform: translateX(0); opacity: 1; }
        to { transform: translateX(100%); opacity: 0; }
    }
    
    mark {
        background: #ffeb3b;
        padding: 2px 4px;
        border-radius: 3px;
        font-weight: bold;
    }
    
    .toast {
        box-shadow: 0 4px 12px rgba(0,0,0,0.2);
    }
`;
document.head.appendChild(style);
