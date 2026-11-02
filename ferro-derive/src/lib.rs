use proc_macro::TokenStream;

#[proc_macro_derive(Data, attributes(ferro))]
pub fn derive_data(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}

#[proc_macro_derive(Record, attributes(key, index, ferro))]
pub fn derive_record(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}
